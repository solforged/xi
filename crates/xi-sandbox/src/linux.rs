//! Linux sandbox: user, mount and network namespaces plus Landlock.

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::net::UdpSocket;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::{
    ABI, Access, AccessFs, BitFlags, PathBeneath, Ruleset, RulesetAttr, RulesetCreatedAttr,
    RulesetError, RulesetStatus, Scope, make_bitflags,
};
use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};
use nix::sys::statvfs::{FsFlags, statvfs};
use nix::unistd::{getgid, getuid, setsid};

use crate::{Error, Network, Spec};

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
    let mut namespaces = CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNS;
    if spec.network == Network::Isolated {
        namespaces |= CloneFlags::CLONE_NEWNET;
    }
    unshare(namespaces).map_err(setup("unshare namespaces"))?;
    fs::write("/proc/self/setgroups", "deny").map_err(setup("deny setgroups"))?;
    fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).map_err(setup("map uid"))?;
    fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).map_err(setup("map gid"))?;
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

    for path in &spec.protected {
        bind_read_only(path)?;
    }
    mount_tmp(spec)?;
    if spec.network == Network::Isolated {
        loopback_up().map_err(setup("bring up loopback"))?;
    }

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

/// Mounts the session tmp at `/tmp`. Writable roots under the host `/tmp`
/// are first bound into the session tmp at the same relative path, so they
/// keep their paths.
fn mount_tmp(spec: &Spec) -> Result<(), Error> {
    let host_tmp = Path::new("/tmp");
    for root in &spec.writable {
        if *root == spec.tmp {
            continue;
        }
        let Ok(relative) = root.strip_prefix(host_tmp) else {
            continue;
        };
        let target: PathBuf = spec.tmp.join(relative);
        let step = || format!("expose {} under /tmp", root.display());
        fs::create_dir_all(&target).map_err(setup(step()))?;
        mount(
            Some(root.as_path()),
            &target,
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REC,
            None::<&str>,
        )
        .map_err(setup(step()))?;
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
