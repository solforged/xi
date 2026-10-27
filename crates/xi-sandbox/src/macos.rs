//! macOS sandbox: a deny-by-default Seatbelt profile installed by sandbox-exec.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::fmt::Write;
use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::unistd::{AccessFlags, access, setsid};

use crate::{Error, Network, Spec};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const BASE_POLICY: &str = include_str!("seatbelt.sbpl");

// DNS and TLS trust evaluation, only with Host networking. These are not the
// keychain services (securityd.xpc / SecurityServer), which remain denied.
const HOST_NETWORK: &str = r#"
(allow network-bind network-inbound (local ip "*:*"))
(allow network-outbound (remote ip "*:*"))
; Native DNS uses this system socket, not a Mach service.
(allow network-outbound
  (remote unix-socket (literal "/private/var/run/mDNSResponder")))
(allow system-socket
  (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow mach-lookup
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd")
  (global-name "com.apple.networkd")
  (global-name "com.apple.trustd.agent")
  (global-name "com.apple.TrustEvaluationAgent"))
(allow sysctl-read (sysctl-name-prefix "net.routetable."))
"#;

pub(crate) fn enter_and_exec(
    spec: &Spec,
    program: &OsStr,
    args: &[OsString],
) -> Result<Infallible, Error> {
    access(SANDBOX_EXEC, AccessFlags::X_OK).map_err(setup("locate /usr/bin/sandbox-exec"))?;
    setsid().map_err(setup("setsid"))?;
    for path in &spec.protected {
        if !path.exists() {
            fs::create_dir_all(path).map_err(setup(format!("create {}", path.display())))?;
        }
    }
    let helper = std::env::current_exe().map_err(setup("locate sandbox helper"))?;
    let profile = profile(spec, &helper)?;
    let target = resolve_program(program).map_err(|source| Error::Exec {
        program: program.to_owned(),
        source,
    })?;

    // The second helper stage preserves errno-based 126/127 for a target that
    // disappears or is denied by Seatbelt. sandbox-exec itself uses status 71
    // for exec failures. All three execs preserve the initial helper's pid/pgid.
    let source = Command::new(SANDBOX_EXEC)
        .args([OsStr::new("-p"), OsStr::new(&profile), OsStr::new("--")])
        .arg(helper)
        .arg("--internal-exec")
        .arg(target)
        .args(args)
        .env("XI_SANDBOX_EXEC_STAGE", "1")
        .env("TMPDIR", &spec.tmp)
        .exec();
    Err(setup("execute /usr/bin/sandbox-exec")(source))
}

/// Resolves PATH before Seatbelt hides its inaccessible entries. Do not
/// canonicalize the final executable: multicall tools depend on argv[0].
fn resolve_program(program: &OsStr) -> io::Result<PathBuf> {
    fn executable(path: &Path) -> io::Result<()> {
        if !path.metadata()?.is_file() {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        access(path, AccessFlags::X_OK).map_err(io::Error::from)
    }
    if program.as_encoded_bytes().contains(&b'/') {
        let path = std::path::absolute(program)?;
        executable(&path)?;
        return Ok(path);
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut denied = None;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        match executable(&candidate) {
            Ok(()) => return std::path::absolute(candidate),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => denied = Some(error),
        }
    }
    Err(denied.unwrap_or_else(|| io::Error::from(io::ErrorKind::NotFound)))
}

fn profile(spec: &Spec, helper: &Path) -> Result<String, Error> {
    let mut policy = String::from(BASE_POLICY);
    let mut readable = BTreeSet::new();
    let mut writable = BTreeSet::new();
    let mut metadata = BTreeSet::new();
    for path in &spec.readable {
        match path.canonicalize() {
            Ok(canonical) => {
                if canonical != *path {
                    // Reading a symlink's text needs file-read-data as well
                    // as metadata; this does not authorize its new target.
                    writeln!(policy, "(allow file-read* (literal {}))", quote(path)?).unwrap();
                }
                readable.insert(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(setup(format!("resolve {}", path.display()))(error)),
        }
        metadata.extend(path.ancestors().map(Path::to_owned));
    }
    for path in spec.writable.iter().chain(std::iter::once(&spec.tmp)) {
        writable.insert(canonicalize(path)?);
        metadata.extend(path.ancestors().map(Path::to_owned));
    }
    readable.extend(writable.iter().cloned());
    for path in &readable {
        writeln!(
            policy,
            "(allow file-read* file-map-executable (subpath {}))",
            quote(path)?
        )
        .unwrap();
        metadata.extend(path.ancestors().map(Path::to_owned));
    }
    // A path written through a root-level link such as /tmp -> private/tmp
    // resolves only if the link itself is readable. That reveals its text
    // and nothing beneath it; the target keeps its own rules.
    for (link, target) in root_links()? {
        if readable.iter().any(|root| root.starts_with(&target)) {
            writeln!(policy, "(allow file-read* (literal {}))", quote(&link)?).unwrap();
        }
    }
    // Allow only this executable, not its containing build or home directory.
    writeln!(
        policy,
        "(allow file-read* file-map-executable (literal {}))",
        quote(helper)?
    )
    .unwrap();
    metadata.extend(helper.ancestors().map(Path::to_owned));
    for path in metadata {
        writeln!(
            policy,
            "(allow file-read-metadata (literal {}))",
            quote(&path)?
        )
        .unwrap();
    }
    for path in writable {
        let path = quote(&path)?;
        writeln!(policy, "(allow file-write* (subpath {path}))").unwrap();
        writeln!(
            policy,
            "(allow network-bind (local unix-socket (subpath {path})))"
        )
        .unwrap();
        writeln!(
            policy,
            "(allow network-outbound (remote unix-socket (subpath {path})))"
        )
        .unwrap();
        writeln!(
            policy,
            "(allow network-inbound (local unix-socket (subpath {path})))"
        )
        .unwrap();
    }
    if spec.network == Network::Host {
        policy.push_str(HOST_NETWORK);
    }
    if spec.network == Network::Isolated
        && let Some(egress) = &spec.egress
    {
        writeln!(
            policy,
            "(allow network-outbound (remote ip \"localhost:{}\"))",
            egress.port
        )
        .unwrap();
    }

    // Denies must come after the allows. Pin ancestors against rename/unlink
    // so moving .git out of the protected pathname cannot bypass the policy.
    let mut pinned = BTreeSet::new();
    for path in &spec.protected {
        let canonical = canonicalize(path)?;
        for path in [path.as_path(), canonical.as_path()] {
            writeln!(policy, "(deny file-write* (subpath {}))", quote(path)?).unwrap();
            pinned.extend(path.ancestors().skip(1).map(Path::to_owned));
        }
    }
    for path in pinned {
        writeln!(
            policy,
            "(deny file-write-unlink (literal {}))",
            quote(&path)?
        )
        .unwrap();
    }
    Ok(policy)
}

fn canonicalize(path: &Path) -> Result<PathBuf, Error> {
    path.canonicalize()
        .map_err(setup(format!("resolve {}", path.display())))
}

/// Symlinks directly under `/`, with their resolved targets.
fn root_links() -> Result<Vec<(PathBuf, PathBuf)>, Error> {
    let mut links = Vec::new();
    for entry in std::fs::read_dir("/").map_err(setup("list /"))? {
        let entry = entry.map_err(setup("list /"))?;
        if entry.file_type().is_ok_and(|kind| kind.is_symlink())
            && let Ok(target) = entry.path().canonicalize()
        {
            links.push((entry.path(), target));
        }
    }
    Ok(links)
}

/// JSON's quoted strings also escape the SBPL string delimiters. Reject
/// control characters and non-UTF-8 rather than risk lossy path authorization.
fn quote(path: &Path) -> Result<String, Error> {
    let text = path
        .to_str()
        .filter(|text| !text.chars().any(char::is_control));
    match text {
        Some(text) => Ok(serde_json::to_string(text).expect("serializing a string cannot fail")),
        None => Err(setup("encode Seatbelt path")(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Seatbelt paths must be UTF-8 without control characters",
        ))),
    }
}

fn setup<E: Into<io::Error>>(step: impl Into<String>) -> impl FnOnce(E) -> Error {
    let step = step.into();
    move |source| Error::Setup {
        step,
        source: source.into(),
    }
}
