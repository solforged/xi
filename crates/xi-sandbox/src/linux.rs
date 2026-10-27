//! Linux sandbox: user, mount and network namespaces plus Landlock.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetError, RulesetStatus, Scope, make_bitflags,
};
use nix::fcntl::{FcntlArg, FdFlag, fcntl};
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::Signal;
use nix::sys::statvfs::{FsFlags, statvfs};
use nix::unistd::{ForkResult, Pid, fork, getgid, getpid, getppid, getuid, setsid};

use crate::{Egress, Error, Network, Spec};

/// Highest Landlock ABI this crate requests. Version 9 adds control over
/// connecting to pathname Unix sockets.
const ABI_TARGET: ABI = ABI::V9;

/// Device nodes usable inside the sandbox, when they exist.
const DEVICES: &[&str] = &[
    "/dev/full",
    "/dev/null",
    "/dev/ptmx",
    "/dev/pts",
    "/dev/random",
    "/dev/tty",
    "/dev/urandom",
    "/dev/zero",
];

/// Replaced by a private tmpfs, then writable.
const SHM: &str = "/dev/shm";

pub(crate) fn enter_and_exec(
    spec: &Spec,
    program: &OsStr,
    args: &[OsString],
) -> Result<Infallible, Error> {
    // A new session detaches the controlling terminal, which closes the
    // TIOCSTI route for pushing input into the user's shell.
    setsid().map_err(setup("setsid"))?;
    for path in &spec.protected {
        if !path.exists() {
            fs::create_dir_all(path).map_err(setup(format!("create {}", path.display())))?;
        }
    }

    let uid = getuid();
    let gid = getgid();
    let mut namespaces = CloneFlags::CLONE_NEWUSER;
    if spec.network == Network::Isolated {
        namespaces |= CloneFlags::CLONE_NEWNET;
    }
    unshare(namespaces).map_err(setup("unshare namespaces"))?;
    fs::write("/proc/self/setgroups", "deny").map_err(setup("deny setgroups"))?;
    fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).map_err(setup("map uid"))?;
    fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).map_err(setup("map gid"))?;
    if spec.network == Network::Isolated {
        loopback_up().map_err(setup("bring up loopback"))?;
        if let Some(egress) = &spec.egress {
            start_egress(egress)?;
        }
    }
    // The forwarder keeps the host mount view, including sockets under
    // /tmp. Only the target enters the namespace whose mounts change.
    unshare(CloneFlags::CLONE_NEWNS).map_err(setup("unshare mount namespace"))?;
    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(setup("make mounts private"))?;
    let shm = Path::new(SHM).is_dir();
    if shm {
        mount(
            Some("tmpfs"),
            SHM,
            Some("tmpfs"),
            MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
            Some("mode=1777"),
        )
        .map_err(setup("mount private /dev/shm"))?;
    }

    // Rules bind to inodes, so open every root before /tmp changes meaning.
    let abi = ABI_TARGET;
    let mut rules = Vec::new();
    for path in &spec.readable {
        if let Some(rule) = open_rule(path, AccessFs::from_read(abi), false)? {
            rules.push(rule);
        }
    }
    for path in &spec.writable {
        rules.extend(open_rule(path, AccessFs::from_all(abi), true)?);
    }
    let device_access = make_bitflags!(AccessFs::{ReadFile | WriteFile | ReadDir | IoctlDev});
    for path in DEVICES {
        rules.extend(open_rule(Path::new(path), device_access, false)?);
    }
    if shm {
        rules.extend(open_rule(Path::new(SHM), AccessFs::from_all(abi), true)?);
    }

    pin_protected_ancestors(spec)?;
    for path in &spec.protected {
        bind_read_only(path)?;
    }
    mount_tmp(spec)?;
    // Binding a writable root leaves an inherited cwd on the covered mount.
    // Re-enter through the new mount tree so relative paths see the guards.
    std::env::set_current_dir(&spec.cwd).map_err(setup("enter sandbox working directory"))?;

    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .add_rules(
            rules
                .into_iter()
                .map(|(fd, access)| Ok::<_, RulesetError>(PathBeneath::new(fd, access))),
        )?
        .restrict_self()?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => {}
        RulesetStatus::PartiallyEnforced if spec.allow_partial => {}
        RulesetStatus::PartiallyEnforced => {
            return Err(Error::NotEnforced(format!(
                "this kernel enforces only part of the Landlock ruleset ({:?}); \
                 full enforcement needs Landlock ABI 9 (Linux 7.1)",
                status.landlock
            )));
        }
        RulesetStatus::NotEnforced => {
            return Err(Error::NotEnforced(format!(
                "Landlock is unavailable ({:?})",
                status.landlock
            )));
        }
    }

    let source = Command::new(program).args(args).exec();
    Err(Error::Exec {
        program: program.to_owned(),
        source,
    })
}

/// Starts a forwarder before mounts and Landlock hide the kernel's socket.
#[allow(unsafe_code)]
fn start_egress(egress: &Egress) -> Result<(), Error> {
    let listener =
        TcpListener::bind(("127.0.0.1", egress.port)).map_err(setup("bind egress forwarder"))?;
    let listener = if listener.as_raw_fd() < 3 {
        let fd = fcntl(&listener, FcntlArg::F_DUPFD_CLOEXEC(3))
            .map_err(setup("preserve egress listener"))?;
        drop(listener);
        // SAFETY: fcntl returned a new, owned TCP listener descriptor.
        unsafe { TcpListener::from_raw_fd(fd) }
    } else {
        listener
    };
    fcntl(&listener, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
        .map_err(setup("mark egress listener close-on-exec"))?;
    let helper = getpid();
    // SAFETY: the sandbox helper is single-threaded and has not installed
    // restrictions yet. The child stays in Rust and never execs a target.
    match unsafe { fork() }.map_err(setup("fork egress forwarder"))? {
        ForkResult::Parent { .. } => {
            drop(listener);
            Ok(())
        }
        ForkResult::Child => {
            let error = run_egress(listener, &egress.socket, helper).unwrap_err();
            eprintln!("xi-sandbox-exec: {error}");
            std::process::exit(crate::EXIT_SETUP_FAILED);
        }
    }
}

fn run_egress(listener: TcpListener, socket: &Path, helper: Pid) -> Result<Infallible, Error> {
    set_pdeathsig(Signal::SIGKILL).map_err(setup("set egress parent death signal"))?;
    // The helper may have exited before the death signal was installed.
    if getppid() != helper {
        std::process::exit(0);
    }
    std::env::set_current_dir("/").map_err(setup("detach egress working directory"))?;
    close_egress_fds(listener.as_raw_fd()).map_err(setup("detach egress file descriptors"))?;
    std::thread::scope(|scope| {
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                continue;
            };
            let _ = std::thread::Builder::new().spawn_scoped(scope, move || {
                if let Ok(upstream) = UnixStream::connect(socket) {
                    relay_egress(stream, upstream);
                }
            });
        }
    });
    std::process::exit(0);
}

/// Keeps only the listener and null standard streams in the forwarder.
#[allow(unsafe_code)]
fn close_egress_fds(listener: libc::c_int) -> io::Result<()> {
    let null = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    for target in 0..=2 {
        // SAFETY: null is open, and target is a standard descriptor. The
        // listener was moved above these descriptors before the fork.
        if unsafe { libc::dup2(null.as_raw_fd(), target) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    drop(null);
    for (first, last) in [
        (3, listener as libc::c_uint - 1),
        (listener as libc::c_uint + 1, libc::c_uint::MAX),
    ] {
        if first > last {
            continue;
        }
        // SAFETY: close_range takes only integer bounds and flags. Neither
        // range contains the listener or the standard descriptors.
        if unsafe { libc::syscall(libc::SYS_close_range, first, last, 0_u32) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn relay_egress(stream: TcpStream, upstream: UnixStream) {
    std::thread::scope(|scope| {
        let relay = std::thread::Builder::new().spawn_scoped(scope, || {
            let _ = io::copy(&mut &stream, &mut &upstream);
            let _ = upstream.shutdown(Shutdown::Write);
        });
        if relay.is_err() {
            return;
        }
        let _ = io::copy(&mut &upstream, &mut &stream);
        let _ = stream.shutdown(Shutdown::Write);
    });
}

/// Opens `path` for a Landlock rule. Directory-only rights are dropped for
/// files. A missing path is an error when `required`, otherwise skipped.
fn open_rule(
    path: &Path,
    access: BitFlags<AccessFs>,
    required: bool,
) -> Result<Option<(File, BitFlags<AccessFs>)>, Error> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => return Ok(None),
        Err(error) => return Err(setup(format!("open {}", path.display()))(error)),
    };
    let metadata = file
        .metadata()
        .map_err(setup(format!("stat {}", path.display())))?;
    let access = if metadata.is_dir() {
        access
    } else {
        access & AccessFs::from_file(ABI_TARGET)
    };
    Ok(Some((file, access)))
}

/// Mount points cannot be renamed or unlinked. Pin the writable ancestors
/// before protecting their children, so moving .git cannot expose a fresh
/// unprotected .git/config at the original path.
fn pin_protected_ancestors(spec: &Spec) -> Result<(), Error> {
    let roots = spec
        .writable
        .iter()
        .map(|path| path.canonicalize().map_err(setup("resolve writable root")))
        .collect::<Result<Vec<_>, _>>()?;
    let mut ancestors = BTreeSet::new();
    for path in &spec.protected {
        let path = path
            .canonicalize()
            .map_err(setup("resolve protected path"))?;
        for ancestor in path.ancestors().skip(1) {
            if !roots.iter().any(|root| ancestor.starts_with(root)) {
                break;
            }
            ancestors.insert(ancestor.to_owned());
        }
    }
    // Parents sort before children. Recursive binds preserve existing
    // submounts, including any explicitly writable nested roots.
    for path in ancestors {
        mount(
            Some(path.as_path()),
            &path,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(setup(format!("pin protected ancestor {}", path.display())))?;
    }
    Ok(())
}

/// Bind-mounts `path` over itself read-only. Remounting inside a user
/// namespace must keep the flags locked by the original mount.
fn bind_read_only(path: &Path) -> Result<(), Error> {
    let step = || format!("protect {}", path.display());
    mount(
        Some(path),
        path,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(setup(step()))?;
    let flags = statvfs(path).map_err(setup(step()))?.flags();
    let mut locked = MsFlags::empty();
    for (fs_flag, ms_flag) in [
        (FsFlags::ST_NOSUID, MsFlags::MS_NOSUID),
        (FsFlags::ST_NODEV, MsFlags::MS_NODEV),
        (FsFlags::ST_NOEXEC, MsFlags::MS_NOEXEC),
        (FsFlags::ST_NOATIME, MsFlags::MS_NOATIME),
        (FsFlags::ST_NODIRATIME, MsFlags::MS_NODIRATIME),
        (FsFlags::ST_RELATIME, MsFlags::MS_RELATIME),
    ] {
        if flags.contains(fs_flag) {
            locked |= ms_flag;
        }
    }
    mount(
        None::<&str>,
        path,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY | locked,
        None::<&str>,
    )
    .map_err(setup(step()))
}

/// Mounts the session tmp at `/tmp`. Writable and readable roots under the
/// host `/tmp` are first bound into the session tmp at the same relative
/// path, so they keep their paths. Bound readable roots are remounted
/// read-only: inside the session tmp, Landlock's write rule for the tmp
/// would otherwise cover them.
fn mount_tmp(spec: &Spec) -> Result<(), Error> {
    let host_tmp = Path::new("/tmp");
    let roots = spec.writable.iter().map(|root| (root, true));
    let roots = roots.chain(spec.readable.iter().map(|root| (root, false)));
    for (root, writable) in roots {
        if *root == spec.tmp {
            continue;
        }
        let Ok(relative) = root.strip_prefix(host_tmp) else {
            continue;
        };
        if relative.as_os_str().is_empty() || !root.exists() {
            continue;
        }
        let target: PathBuf = spec.tmp.join(relative);
        let step = || format!("expose {} under /tmp", root.display());
        if root.is_dir() {
            fs::create_dir_all(&target).map_err(setup(step()))?;
        } else {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(setup(step()))?;
            }
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&target)
                .map_err(setup(step()))?;
        }
        mount(
            Some(root.as_path()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(setup(step()))?;
        if !writable && !spec.writable.iter().any(|w| root.starts_with(w)) {
            bind_read_only(&target)?;
        }
    }
    mount(
        Some(spec.tmp.as_path()),
        host_tmp,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(setup("mount session tmp at /tmp"))
}

/// Sets IFF_UP on `lo`. A new network namespace starts with it down.
#[allow(unsafe_code)]
fn loopback_up() -> io::Result<()> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    // SAFETY: `ifreq` is plain old data; all-zero is a valid value.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    for (dst, src) in request.ifr_name.iter_mut().zip(b"lo") {
        *dst = *src as libc::c_char;
    }
    // SAFETY: the fd is open for the duration of the call, and SIOCGIFFLAGS
    // writes only `ifr_flags` inside the `ifreq` we own.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCGIFFLAGS, &mut request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: SIOCGIFFLAGS initialized the `ifru_flags` member of the union.
    unsafe {
        request.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
    }
    // SAFETY: as above; SIOCSIFFLAGS reads the `ifreq`.
    if unsafe { libc::ioctl(socket.as_raw_fd(), libc::SIOCSIFFLAGS, &request) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn setup<E: Into<io::Error>>(step: impl Into<String>) -> impl FnOnce(E) -> Error {
    let step = step.into();
    move |source| Error::Setup {
        step,
        source: source.into(),
    }
}
