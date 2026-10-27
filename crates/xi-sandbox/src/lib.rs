//! Runs tool processes inside an OS sandbox.
//!
//! The kernel never sandboxes itself. It starts tools through the
//! `xi-sandbox-exec` helper, which enters the sandbox described by a [`Spec`]
//! and then execs the target program. Restricting a separate single-threaded
//! helper keeps non-async-signal-safe setup out of the window between `fork`
//! and `exec` in the multi-threaded kernel. With [`Spec::enabled`] false,
//! [`Spec::command`] launches directly without the helper or OS restrictions.
//!
//! On Linux the helper uses a user namespace, a mount namespace, optionally a
//! network namespace, and Landlock:
//!
//! - Landlock limits filesystem access, including connecting to pathname
//!   Unix sockets, to the spec's readable and writable roots and explicit
//!   connect-only socket grants. It also stops
//!   the sandbox from signalling or reaching abstract Unix sockets outside
//!   it.
//! - [`Spec::protected`] paths are bind-mounted read-only over themselves,
//!   so they stay read-only inside writable roots. Writable roots inside a
//!   protected path are bound first, so they stay writable as submounts.
//! - [`Spec::tmp`] is mounted at `/tmp`.
//! - [`Network::Isolated`] gives the process a network namespace with only
//!   loopback, so no host is reachable. With [`Spec::egress`], a forwarder
//!   listens on the proxy port and relays connections to the kernel's Unix
//!   socket outside the sandbox's filesystem restrictions.
//!
//! On macOS the helper uses Seatbelt through `/usr/bin/sandbox-exec`.
//! Filesystem and Unix socket access follow the same roots, with additional
//! connect-only access to [`Spec::sockets`]. Isolated
//! networking denies all IP traffic, including loopback, except the exact
//! IPv4 loopback proxy port when egress is configured. The session tmp is
//! exposed through `TMPDIR`, not mounted over `/tmp`.
//!
//! Other platforms fail closed.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Workspace-relative directory for git worktrees, writable by default
/// inside the protected `.xi`.
pub const WORKTREES: &str = ".xi/worktrees";

/// Exit status of the helper when it fails before running the program.
pub const EXIT_SETUP_FAILED: i32 = 125;
/// Exit status of the helper when the program exists but cannot run.
pub const EXIT_CANNOT_EXECUTE: i32 = 126;
/// Exit status of the helper when the program does not exist.
pub const EXIT_NOT_FOUND: i32 = 127;

/// System paths readable inside the sandbox, when they exist.
#[cfg(not(target_os = "macos"))]
const SYSTEM_READABLE: &[&str] = &[
    "/bin",
    "/etc",
    "/lib",
    "/lib32",
    "/lib64",
    "/nix/store",
    "/opt",
    "/proc",
    "/run/current-system",
    "/sbin",
    "/sys",
    "/usr",
];

// Do not allow all of /System: /System/Volumes/Data exposes the home tree.
#[cfg(target_os = "macos")]
const SYSTEM_READABLE: &[&str] = &[
    "/System/Library",
    "/System/Cryptexes",
    "/System/Developer",
    "/System/DriverKit",
    "/System/iOSSupport",
    "/System/x86Support",
    "/usr",
    "/bin",
    "/sbin",
    "/Library/Apple",
    "/Library/Frameworks",
    "/Library/Preferences/Logging",
    "/Library/Preferences/com.apple.dt.Xcode.plist",
    "/Library/Developer/CommandLineTools",
    "/Library/Developer/PrivateFrameworks",
    "/Applications/Xcode.app",
    "/private/etc",
    "/etc",
    "/private/var/db/timezone",
    "/private/var/db/xcode_select_link",
    "/private/var/db/dyld",
    "/private/var/select/sh",
    "/private/var/select/developer_dir",
    "/var/select/developer_dir",
    "/var/db/xcode_select_link",
];

/// Environment variables passed through by default. Everything else is
/// dropped, including credentials and agent sockets.
const DEFAULT_ENV: &[&str] = &[
    "COLORTERM",
    "HOME",
    "LANG",
    "LANGUAGE",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LOCALE_ARCHIVE",
    "LOGNAME",
    "NIX_SSL_CERT_FILE",
    "NO_COLOR",
    "PATH",
    "SSL_CERT_FILE",
    "TERM",
    "TZ",
    "USER",
];

/// Errors from building or entering a sandbox.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Every path in a spec must be absolute.
    #[error("{field} path must be absolute: {}", path.display())]
    RelativePath {
        /// The spec field holding the path.
        field: &'static str,
        /// The offending path.
        path: PathBuf,
    },
    /// The spec could not be serialized for the helper.
    #[error("cannot encode sandbox spec: {0}")]
    Encode(#[source] serde_json::Error),
    /// The platform has no sandbox implementation.
    #[error("sandboxing is not supported on this platform")]
    Unsupported,
    /// A setup step failed before the program ran.
    #[error("{step}: {source}")]
    Setup {
        /// What the helper was doing.
        step: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Landlock rejected the ruleset.
    #[cfg(target_os = "linux")]
    #[error("landlock: {0}")]
    Landlock(#[from] landlock::RulesetError),
    /// The kernel could not enforce every requested restriction.
    #[error("{0}")]
    NotEnforced(String),
    /// The program could not be executed.
    #[error("cannot execute {}: {source}", program.to_string_lossy())]
    Exec {
        /// The program.
        program: OsString,
        /// The `execve` error.
        #[source]
        source: io::Error,
    },
}

impl Error {
    /// The helper's exit status for this error.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Exec { source, .. } if source.kind() == io::ErrorKind::NotFound => EXIT_NOT_FOUND,
            Self::Exec { .. } => EXIT_CANNOT_EXECUTE,
            _ => EXIT_SETUP_FAILED,
        }
    }
}

/// Network access for the sandboxed process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Network {
    /// Linux: a private network namespace with only loopback.
    /// macOS: no IP networking except a configured egress proxy.
    #[default]
    Isolated,
    /// The host network, unrestricted.
    Host,
}

/// The session's egress proxy, reachable from inside an isolated sandbox.
///
/// Sandboxed processes get `HTTP_PROXY` and friends pointing at
/// `127.0.0.1:port`. On macOS the kernel listens there and the Seatbelt
/// profile allows exactly that destination. On Linux the helper starts a
/// forwarder on that port inside the network namespace which relays each
/// connection to `socket`, the Unix socket the kernel listens on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Egress {
    /// Loopback TCP port the sandboxed process may connect to.
    pub port: u16,
    /// Linux: the kernel's Unix socket the in-namespace forwarder relays to.
    /// Unused on macOS.
    pub socket: PathBuf,
}

/// What a sandboxed process may access.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// Whether [`Self::command`] uses the OS sandbox helper. Enabled by default.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Roots that are readable, writable and executable. A root inside a
    /// [`Self::protected`] path stays writable; it is created as an empty
    /// directory when missing, since the sandbox cannot create it.
    pub writable: Vec<PathBuf>,
    /// Roots that are readable and executable. Missing paths are skipped.
    pub readable: Vec<PathBuf>,
    /// Paths kept read-only even inside writable roots. A missing path is
    /// created as an empty directory first, so the sandbox cannot create it.
    pub protected: Vec<PathBuf>,
    /// Exact pathname Unix sockets that processes may connect to. This grants
    /// no filesystem writes, socket binding, or access to sibling sockets.
    #[serde(default)]
    pub sockets: Vec<PathBuf>,
    /// Linux: directory mounted at `/tmp`; writable roots under the host
    /// `/tmp` stay reachable at their original paths.
    /// macOS: writable directory used as `TMPDIR`, without a remount.
    pub tmp: PathBuf,
    /// Working directory of the program.
    pub cwd: PathBuf,
    /// Network access.
    pub network: Network,
    /// Names of environment variables passed through from the caller.
    pub env: Vec<String>,
    /// Variables set to fixed values, applied after the passthrough list.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
    /// The egress proxy. `None` means no host network access under
    /// [`Network::Isolated`]. Ignored under [`Network::Host`].
    #[serde(default)]
    pub egress: Option<Egress>,
    /// Run even when the kernel can enforce only some Landlock restrictions.
    /// Ignored on macOS, where Seatbelt must install the complete policy.
    pub allow_partial: bool,
}

fn enabled() -> bool {
    true
}

impl Spec {
    /// The default sandbox for a workspace: the workspace and `tmp` are
    /// writable, system paths are readable, git hooks, git config and `.xi`
    /// are protected, and the network is isolated. `.xi/worktrees` stays
    /// writable inside the protected `.xi`, so tools can delegate work to
    /// git worktrees there.
    #[must_use]
    pub fn new(workspace: impl Into<PathBuf>, tmp: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let tmp = tmp.into();
        let git = workspace.join(".git");
        let mut protected = Vec::new();
        if git.join("config").is_file() {
            protected.push(git.join("config"));
        }
        if git.is_dir() {
            protected.push(git.join("hooks"));
        }
        protected.push(workspace.join(".xi"));
        Self {
            enabled: true,
            writable: vec![workspace.clone(), tmp.clone(), workspace.join(WORKTREES)],
            readable: SYSTEM_READABLE
                .iter()
                .map(PathBuf::from)
                .filter(|path| path.exists())
                .collect(),
            protected,
            sockets: Vec::new(),
            tmp,
            cwd: workspace,
            network: Network::Isolated,
            env: DEFAULT_ENV.iter().map(|&name| name.to_owned()).collect(),
            vars: BTreeMap::new(),
            allow_partial: false,
            egress: None,
        }
    }

    /// Writable roots that lie inside a protected path.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn nested_writable(&self) -> impl Iterator<Item = &PathBuf> {
        self.writable
            .iter()
            .filter(|root| self.protected.iter().any(|p| root.starts_with(p)))
    }

    /// Creates missing protected paths and nested writable roots as empty
    /// directories before the sandbox exists, so it cannot create them.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn create_guarded(&self) -> Result<(), Error> {
        for path in self.protected.iter().chain(self.nested_writable()) {
            if !path.exists() {
                std::fs::create_dir_all(path).map_err(|source| Error::Setup {
                    step: format!("create {}", path.display()),
                    source,
                })?;
            }
        }
        Ok(())
    }

    /// Checks that every path is absolute and socket grants name Unix sockets.
    ///
    /// # Errors
    /// Names the first relative path or invalid socket.
    pub fn validate(&self) -> Result<(), Error> {
        let fields = [
            ("writable", self.writable.as_slice()),
            ("readable", self.readable.as_slice()),
            ("protected", self.protected.as_slice()),
            ("sockets", self.sockets.as_slice()),
            ("tmp", std::slice::from_ref(&self.tmp)),
            ("cwd", std::slice::from_ref(&self.cwd)),
        ];
        for (field, paths) in fields {
            if let Some(path) = paths.iter().find(|path| !path.is_absolute()) {
                return Err(Error::RelativePath {
                    field,
                    path: path.clone(),
                });
            }
        }
        #[cfg(unix)]
        for socket in &self.sockets {
            use std::os::unix::fs::FileTypeExt;
            let check = || {
                if socket.metadata()?.file_type().is_socket() {
                    Ok(())
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "not a Unix socket",
                    ))
                }
            };
            check().map_err(|source| Error::Setup {
                step: format!("validate socket {}", socket.display()),
                source,
            })?;
        }
        Ok(())
    }

    /// Builds a command that runs `program` through the helper binary at
    /// `helper`, or directly when [`Self::enabled`] is false. Direct commands
    /// get their own process group on Unix and no egress proxy variables.
    /// The environment is cleared except for [`Self::env`], then egress proxy
    /// variables and [`Self::vars`] are set, in that order. `TMPDIR` is `/tmp`
    /// on sandboxed Linux and [`Self::tmp`] otherwise. Variables added to the
    /// returned command afterwards pass through too.
    ///
    /// # Errors
    /// Fails for relative paths, invalid sockets or an unencodable spec.
    pub fn command<I, S>(
        &self,
        helper: &Path,
        program: impl AsRef<OsStr>,
        args: I,
    ) -> Result<Command, Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.validate()?;
        let mut command = if self.enabled {
            let spec = serde_json::to_string(self).map_err(Error::Encode)?;
            let mut command = Command::new(helper);
            command.arg(spec).arg(program);
            command
        } else {
            let mut command = Command::new(program);
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                command.process_group(0);
            }
            command
        };
        command.args(args).current_dir(&self.cwd).env_clear();
        for (name, value) in std::env::vars_os() {
            if self.env.iter().any(|allowed| name == OsStr::new(allowed)) {
                command.env(name, value);
            }
        }
        if self.enabled
            && self.network == Network::Isolated
            && let Some(egress) = &self.egress
        {
            let proxy = format!("http://127.0.0.1:{}", egress.port);
            for name in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "http_proxy",
                "https_proxy",
                "ALL_PROXY",
            ] {
                command.env(name, &proxy);
            }
            command.env("NODE_USE_ENV_PROXY", "1");
        }
        command.envs(&self.vars);
        #[cfg(target_os = "macos")]
        command.env("TMPDIR", &self.tmp);
        #[cfg(not(target_os = "macos"))]
        command.env(
            "TMPDIR",
            if self.enabled {
                Path::new("/tmp")
            } else {
                &self.tmp
            },
        );
        Ok(command)
    }
}

/// Enters the sandbox described by `spec` and replaces the current process
/// with `program`. Only returns on failure. Called by the helper binary.
///
/// # Errors
/// Fails if the spec is invalid, any isolation step fails, the kernel cannot
/// enforce the restrictions, or the program cannot be executed.
pub fn enter_and_exec(
    spec: &Spec,
    program: &OsStr,
    args: &[OsString],
) -> Result<Infallible, Error> {
    spec.validate()?;
    #[cfg(target_os = "linux")]
    return linux::enter_and_exec(spec, program, args);
    #[cfg(target_os = "macos")]
    return macos::enter_and_exec(spec, program, args);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (program, args);
        Err(Error::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_is_enabled_for_new_and_legacy_specs() {
        let spec = Spec::new("/workspace", "/tmp/xi-sandbox-test");
        assert!(spec.enabled);
        let mut wire = serde_json::to_value(&spec).unwrap();
        wire.as_object_mut().unwrap().remove("enabled");
        wire.as_object_mut().unwrap().remove("sockets");
        let legacy: Spec = serde_json::from_value(wire).unwrap();
        assert!(legacy.enabled);
        assert!(legacy.sockets.is_empty());
        let command = spec
            .command(Path::new("/helper"), "/bin/true", ["arg"])
            .unwrap();
        assert_eq!(command.get_program(), "/helper");
    }

    #[cfg(unix)]
    #[test]
    fn socket_grants_reject_directories_and_relative_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = Spec::new("/workspace", "/tmp/session");
        spec.sockets.push(root.path().to_owned());
        assert!(
            spec.validate()
                .unwrap_err()
                .to_string()
                .contains("not a Unix socket")
        );
        spec.sockets = vec!["relative.sock".into()];
        assert!(matches!(
            spec.validate(),
            Err(Error::RelativePath {
                field: "sockets",
                ..
            })
        ));
    }

    #[test]
    fn disabled_sandbox_launches_directly_without_injecting_a_proxy() {
        let mut spec = Spec::new("/workspace", "/tmp/xi-sandbox-test");
        spec.enabled = false;
        spec.env.clear();
        spec.egress = Some(Egress {
            port: 3128,
            socket: "/tmp/xi-egress.sock".into(),
        });
        spec.vars.insert("EXTRA".into(), "value".into());
        let command = spec
            .command(Path::new("/missing-helper"), "/bin/echo", ["hello"])
            .unwrap();
        assert_eq!(command.get_program(), "/bin/echo");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["hello"]);
        assert_eq!(command.get_current_dir(), Some(Path::new("/workspace")));
        let environment = command.get_envs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            environment[OsStr::new("TMPDIR")],
            Some(spec.tmp.as_os_str())
        );
        assert_eq!(environment[OsStr::new("EXTRA")], Some(OsStr::new("value")));
        for name in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
            "NODE_USE_ENV_PROXY",
        ] {
            assert!(!environment.contains_key(OsStr::new(name)), "{name}");
        }
    }

    #[test]
    fn command_sets_proxy_environment_only_for_isolated_egress() {
        let mut spec = Spec::new("/workspace", "/tmp/xi-sandbox-test");
        spec.env.clear();
        let proxy_names = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "ALL_PROXY",
            "NODE_USE_ENV_PROXY",
        ];
        for network in [Network::Isolated, Network::Host] {
            for egress in [
                None,
                Some(Egress {
                    port: 3128,
                    socket: PathBuf::from("/tmp/xi-egress.sock"),
                }),
            ] {
                spec.network = network;
                spec.egress = egress;
                let command = spec
                    .command(Path::new("/helper"), "/bin/true", [""; 0])
                    .unwrap();
                let environment = command.get_envs().collect::<BTreeMap<_, _>>();
                for name in proxy_names {
                    let expected = if network == Network::Isolated && spec.egress.is_some() {
                        Some(OsStr::new(if name == "NODE_USE_ENV_PROXY" {
                            "1"
                        } else {
                            "http://127.0.0.1:3128"
                        }))
                    } else {
                        None
                    };
                    assert_eq!(
                        environment.get(OsStr::new(name)).copied().flatten(),
                        expected,
                        "{network:?}: {name}"
                    );
                }
            }
        }
        spec.network = Network::Isolated;
        spec.vars
            .insert("HTTPS_PROXY".into(), "http://override:8080".into());
        spec.vars.insert("NODE_USE_ENV_PROXY".into(), "0".into());
        let command = spec
            .command(Path::new("/helper"), "/bin/true", [""; 0])
            .unwrap();
        let environment = command.get_envs().collect::<BTreeMap<_, _>>();
        assert_eq!(
            environment[OsStr::new("HTTPS_PROXY")],
            Some(OsStr::new("http://override:8080"))
        );
        assert_eq!(
            environment[OsStr::new("NODE_USE_ENV_PROXY")],
            Some(OsStr::new("0"))
        );
    }
}
