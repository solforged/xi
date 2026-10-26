//! Runs tool processes inside an OS sandbox.
//!
//! The kernel never sandboxes itself. It starts tools through the
//! `xi-sandbox-exec` helper, which enters the sandbox described by a [`Spec`]
//! and then execs the target program. Restricting a separate single-threaded
//! helper keeps non-async-signal-safe setup out of the window between `fork`
//! and `exec` in the multi-threaded kernel.
//!
//! On Linux the helper uses a user namespace, a mount namespace, optionally a
//! network namespace, and Landlock:
//!
//! - Landlock limits filesystem access, including connecting to pathname
//!   Unix sockets, to the spec's readable and writable roots. It also stops
//!   the sandbox from signalling or reaching abstract Unix sockets outside
//!   it.
//! - [`Spec::protected`] paths are bind-mounted read-only over themselves,
//!   so they stay read-only inside writable roots.
//! - [`Spec::tmp`] is mounted at `/tmp`.
//! - [`Network::Isolated`] gives the process a network namespace with only
//!   loopback, so no host is reachable.
//!
//! On macOS the helper uses Seatbelt through `/usr/bin/sandbox-exec`.
//! Filesystem and Unix socket access follow the same roots. Isolated
//! networking denies all IP traffic, including loopback. The session tmp
//! is exposed through `TMPDIR`, not mounted over `/tmp`.
//!
//! Other platforms fail closed.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

use std::convert::Infallible;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

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
    /// macOS: no IP networking, including loopback inside the sandbox.
    #[default]
    Isolated,
    /// The host network, unrestricted.
    Host,
}

/// What a sandboxed process may access.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    /// Roots that are readable, writable and executable.
    pub writable: Vec<PathBuf>,
    /// Roots that are readable and executable. Missing paths are skipped.
    pub readable: Vec<PathBuf>,
    /// Paths kept read-only even inside writable roots. A missing path is
    /// created as an empty directory first, so the sandbox cannot create it.
    pub protected: Vec<PathBuf>,
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
    /// Run even when the kernel can enforce only some Landlock restrictions.
    /// Ignored on macOS, where Seatbelt must install the complete policy.
    pub allow_partial: bool,
}

impl Spec {
    /// The default sandbox for a workspace: the workspace and `tmp` are
    /// writable, system paths are readable, git hooks, git config and `.xi`
    /// are protected, and the network is isolated.
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
            writable: vec![workspace.clone(), tmp.clone()],
            readable: SYSTEM_READABLE
                .iter()
                .map(PathBuf::from)
                .filter(|path| path.exists())
                .collect(),
            protected,
            tmp,
            cwd: workspace,
            network: Network::Isolated,
            env: DEFAULT_ENV.iter().map(|&name| name.to_owned()).collect(),
            allow_partial: false,
        }
    }

    /// Checks that every path is absolute.
    ///
    /// # Errors
    /// Names the first relative path.
    pub fn validate(&self) -> Result<(), Error> {
        let fields = [
            ("writable", self.writable.as_slice()),
            ("readable", self.readable.as_slice()),
            ("protected", self.protected.as_slice()),
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
        Ok(())
    }

    /// Builds a command that runs `program` inside this sandbox through the
    /// helper binary at `helper`. The environment is cleared except for
    /// [`Self::env`]. `TMPDIR` is `/tmp` on Linux and [`Self::tmp`] on macOS.
    /// Variables added to the returned command afterwards pass through too.
    ///
    /// # Errors
    /// Fails if a path is relative or the spec cannot be encoded.
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
        let spec = serde_json::to_string(self).map_err(Error::Encode)?;
        let mut command = Command::new(helper);
        command
            .arg(spec)
            .arg(program)
            .args(args)
            .current_dir(&self.cwd)
            .env_clear();
        for (name, value) in std::env::vars_os() {
            if self.env.iter().any(|allowed| name == OsStr::new(allowed)) {
                command.env(name, value);
            }
        }
        #[cfg(target_os = "macos")]
        command.env("TMPDIR", &self.tmp);
        #[cfg(not(target_os = "macos"))]
        command.env("TMPDIR", "/tmp");
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
