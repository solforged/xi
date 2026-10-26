//! `xi-sandbox-exec <spec-json> <program> [args...]`
//!
//! Enters the sandbox described by the spec, then execs the program. Exits
//! with 125 if setup fails, 126 if the program cannot run, and 127 if it does
//! not exist. On macOS, failures inside Apple's profile installation retain
//! `sandbox-exec`'s native exit status.

use std::ffi::OsString;
use std::process::exit;

use xi_sandbox::{EXIT_SETUP_FAILED, Spec};

fn main() {
    let mut args = std::env::args_os().skip(1).peekable();
    #[cfg(target_os = "macos")]
    if args.peek().map(OsString::as_os_str) == Some(std::ffi::OsStr::new("--internal-exec")) {
        use std::os::unix::process::CommandExt;

        // This marker prevents accidental direct use, not hostile callers:
        // callers already have the authority to execute an unsandboxed tool.
        if std::env::var_os("XI_SANDBOX_EXEC_STAGE").as_deref() != Some(std::ffi::OsStr::new("1")) {
            eprintln!("xi-sandbox: internal exec stage requires the sandbox launcher");
            exit(EXIT_SETUP_FAILED);
        }
        args.next();
        let Some(program) = args.next() else {
            exit(EXIT_SETUP_FAILED);
        };
        let source = std::process::Command::new(&program)
            .args(args)
            .env_remove("XI_SANDBOX_EXEC_STAGE")
            .exec();
        let error = xi_sandbox::Error::Exec { program, source };
        eprintln!("xi-sandbox: {error}");
        exit(error.exit_code());
    }
    let (Some(spec), Some(program)) = (args.next(), args.next()) else {
        eprintln!("usage: xi-sandbox-exec <spec-json> <program> [args...]");
        exit(EXIT_SETUP_FAILED);
    };
    let spec: Spec = match serde_json::from_slice(spec.as_encoded_bytes()) {
        Ok(spec) => spec,
        Err(error) => {
            eprintln!("xi-sandbox: invalid spec: {error}");
            exit(EXIT_SETUP_FAILED);
        }
    };
    let rest: Vec<OsString> = args.collect();
    match xi_sandbox::enter_and_exec(&spec, &program, &rest) {
        Ok(never) => match never {},
        Err(error) => {
            eprintln!("xi-sandbox: {error}");
            exit(error.exit_code());
        }
    }
}
