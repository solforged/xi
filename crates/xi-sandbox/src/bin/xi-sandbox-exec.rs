//! `xi-sandbox-exec <spec-json> <program> [args...]`
//!
//! Enters the sandbox described by the spec, then execs the program. Exits
//! with 125 if setup fails, 126 if the program cannot run, and 127 if it does
//! not exist.

use std::ffi::OsString;
use std::process::exit;

use xi_sandbox::{EXIT_SETUP_FAILED, Spec};

fn main() {
    let mut args = std::env::args_os().skip(1);
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
