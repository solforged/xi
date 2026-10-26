//! Linux namespace behavior. Needs Landlock ABI 9 and user namespaces.
#![cfg(target_os = "linux")]

mod common;

use common::{Fixture, run_probe, sh, text};

#[test]
fn isolated_network_reaches_its_own_loopback() {
    let output = run_probe(&Fixture::new().spec(), "loopback:");
    assert!(output.status.success(), "{}", text(&output.stderr));
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
