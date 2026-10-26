//! Shared subprocess fixtures and network probes.

#![allow(dead_code)]

use std::io;
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use xi_sandbox::Spec;

pub const HELPER: &str = env!("CARGO_BIN_EXE_xi-sandbox-exec");
const PROBE_ENV: &str = "XI_SANDBOX_PROBE";

pub struct Fixture {
    _root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub tmp: PathBuf,
    pub outside: PathBuf,
}

impl Fixture {
    pub fn in_dir(parent: &Path) -> Self {
        let root = tempfile::tempdir_in(parent).unwrap();
        let canonical = root.path().canonicalize().unwrap();
        let [workspace, tmp, outside] =
            ["workspace", "tmp", "outside"].map(|name| canonical.join(name));
        for dir in [&workspace, &tmp, &outside] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(outside.join("secret"), "token").unwrap();
        Self {
            _root: root,
            workspace,
            tmp,
            outside,
        }
    }

    pub fn new() -> Self {
        #[cfg(target_os = "linux")]
        return Self::in_dir(Path::new("/var/tmp"));
        #[cfg(target_os = "macos")]
        Self::in_dir(Path::new(env!("CARGO_TARGET_TMPDIR")))
    }

    pub fn spec(&self) -> Spec {
        Spec::new(&self.workspace, &self.tmp)
    }
}

pub fn sh(spec: &Spec, script: &str) -> Output {
    spec.command(Path::new(HELPER), "/bin/sh", ["-c", script])
        .unwrap()
        .output()
        .unwrap()
}

pub fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub fn run_probe(spec: &Spec, action: &str) -> Output {
    // Linux remounts /tmp, which may contain Cargo's target directory. Put
    // the probe in the workspace so it survives that remount.
    let exe = spec.cwd.join(".xi-test-probe");
    if !exe.exists() {
        std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    }
    let mut command: Command = spec
        .command(
            Path::new(HELPER),
            &exe,
            [
                "common::probe",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ],
        )
        .unwrap();
    command.env(PROBE_ENV, action).output().unwrap()
}

#[test]
fn probe() {
    let Ok(action) = std::env::var(PROBE_ENV) else {
        return;
    };
    let result: io::Result<()> = match action.split_once(':') {
        Some(("unix", path)) => UnixStream::connect(path).map(drop),
        Some(("unix-bind", path)) => UnixListener::bind(path).map(drop),
        Some(("tcp", address)) => TcpStream::connect(address).map(drop),
        Some(("udp", address)) => UdpSocket::bind("0.0.0.0:0")
            .and_then(|socket| socket.send_to(b"probe", address))
            .map(drop),
        Some(("loopback", _)) => TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| TcpStream::connect(listener.local_addr()?))
            .map(drop),
        Some(("ipv6", _)) => TcpListener::bind("[::1]:0")
            .and_then(|listener| TcpStream::connect(listener.local_addr()?))
            .map(drop),
        Some(("session", _)) => {
            assert_eq!(nix::unistd::getpid(), nix::unistd::getpgrp());
            assert_eq!(nix::unistd::getsid(None).unwrap(), nix::unistd::getpid());
            assert!(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/tty")
                    .is_err()
            );
            Ok(())
        }
        _ => panic!("unknown probe {action}"),
    };
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("probe failed: {error}");
            std::process::exit(3);
        }
    }
}
