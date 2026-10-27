//! Linux namespace behavior. Needs Landlock ABI 9 and user namespaces.
#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::{Fixture, probe_command, run_probe, sh, text};
use nix::errno::Errno;
use nix::sys::signal::killpg;
use nix::unistd::Pid;
use xi_sandbox::Egress;

#[test]
fn isolated_network_reaches_its_own_loopback() {
    let output = run_probe(&Fixture::new().spec(), "loopback:");
    assert!(output.status.success(), "{}", text(&output.stderr));
}

#[test]
fn isolated_egress_relays_to_a_host_socket_and_exits_with_its_target() {
    let fixture = Fixture::new();
    // The parent remounts /tmp after forking the forwarder. Its child must
    // retain the host mount view to reach a socket beneath that directory.
    let host = tempfile::tempdir_in("/tmp").unwrap();
    let socket = host.path().join("egress.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = [0; 4];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"ping");
        stream.write_all(b"pong").unwrap();
        let mut remaining = Vec::new();
        stream.read_to_end(&mut remaining).unwrap();
        assert!(remaining.is_empty());
    });
    let mut spec = fixture.spec();
    spec.egress = Some(Egress { port: 3128, socket });
    let child = probe_command(&spec, "echo:127.0.0.1:3128")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let group = Pid::from_raw(child.id().try_into().unwrap());
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", text(&output.stderr));
    server.join().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match killpg(group, None) {
            Err(Errno::ESRCH) => break,
            result if Instant::now() >= deadline => {
                panic!("egress process group {group} survived target exit: {result:?}");
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[test]
fn tmp_is_private_to_the_session() {
    let fixture = Fixture::new();
    let host_file = tempfile::NamedTempFile::new_in("/tmp").unwrap();
    let output = sh(
        &fixture.spec(),
        &format!(
            "test ! -e {} && echo inside > /tmp/note && echo $TMPDIR",
            host_file.path().display()
        ),
    );
    assert_eq!(text(&output.stdout), "/tmp\n", "{}", text(&output.stderr));
    assert_eq!(
        std::fs::read_to_string(fixture.tmp.join("note")).unwrap(),
        "inside\n"
    );
}

#[test]
fn readable_roots_under_host_tmp_keep_their_path_and_stay_read_only() {
    let fixture = Fixture::new();
    let shared = tempfile::tempdir_in("/tmp").unwrap();
    let file = shared.path().join("config");
    std::fs::write(&file, "kept\n").unwrap();
    let mut spec = fixture.spec();
    spec.readable.push(shared.path().to_owned());
    let output = sh(
        &spec,
        &format!(
            "cat {0} && ! echo changed > {0} && ! touch {1}/new",
            file.display(),
            shared.path().display()
        ),
    );
    assert!(output.status.success(), "{}", text(&output.stderr));
    assert_eq!(text(&output.stdout), "kept\n");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "kept\n");
}
