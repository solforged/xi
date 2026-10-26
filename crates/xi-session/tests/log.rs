//! Behavior of the session log across appends, reopen, crashes and branches.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use xi_protocol::event::ModelRef;
use xi_protocol::{Block, EventId, EventKind, SessionId};
use xi_session::{Error, SessionLog, Violation};

fn started() -> EventKind {
    EventKind::SessionStarted {
        session: SessionId::generate(),
        cwd: "/work".into(),
        model: ModelRef {
            provider: "anthropic".into(),
            model: "claude".into(),
        },
        xi_version: "0.0.0".into(),
    }
}

fn user(text: &str) -> EventKind {
    EventKind::UserMessage {
        content: vec![Block::Text { text: text.into() }],
    }
}

fn session_file(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("session.jsonl")
}

fn append_raw(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn branches_survive_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut log = SessionLog::create(&path, started()).unwrap();
    let root = log.root().id;
    let u1 = log.append(root, user("one")).unwrap().id;
    let a1 = log.append(u1, user("first branch")).unwrap().id;
    let b1 = log.append(u1, user("second branch")).unwrap().id;
    drop(log);

    let log = SessionLog::open(&path).unwrap();
    assert_eq!(log.events().len(), 4);
    assert_eq!(log.children(u1), [a1, b1]);
    let leaves: Vec<_> = log.leaves().map(|event| event.id).collect();
    assert_eq!(leaves, [a1, b1]);
    let lineage: Vec<_> = log.lineage(b1).unwrap().iter().map(|e| e.id).collect();
    assert_eq!(lineage, [root, u1, b1]);
}

#[test]
fn torn_tail_is_dropped_and_appends_continue() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut log = SessionLog::create(&path, started()).unwrap();
    let root = log.root().id;
    log.append(root, user("kept")).unwrap();
    drop(log);
    append_raw(&path, br#"{"id":"0199"#);

    let mut log = SessionLog::open(&path).unwrap();
    assert_eq!(log.dropped_tail_bytes(), 11);
    assert_eq!(log.events().len(), 2);
    log.append(root, user("after recovery")).unwrap();
    drop(log);

    let log = SessionLog::open(&path).unwrap();
    assert_eq!(log.dropped_tail_bytes(), 0);
    assert_eq!(log.events().len(), 3);
}

#[test]
fn malformed_complete_line_reports_its_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    drop(SessionLog::create(&path, started()).unwrap());
    append_raw(&path, b"not json\n");

    let error = SessionLog::open(&path).unwrap_err();
    assert!(
        matches!(error, Error::Corrupt { line: 2, .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn out_of_order_parent_on_disk_is_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut log = SessionLog::create(&path, started()).unwrap();
    let root = log.root().id;
    log.append(root, user("child")).unwrap();
    drop(log);
    let text = fs::read_to_string(&path).unwrap();
    let mut lines: Vec<_> = text.lines().collect();
    lines.swap(0, 1);
    fs::write(&path, lines.join("\n") + "\n").unwrap();

    let error = SessionLog::open(&path).unwrap_err();
    assert!(
        matches!(error, Error::Corrupt { line: 1, .. }),
        "unexpected error: {error}"
    );
}

#[test]
fn rejected_append_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut log = SessionLog::create(&path, started()).unwrap();
    let root = log.root().id;
    let size = fs::metadata(&path).unwrap().len();

    let unknown = EventId::generate();
    let error = log.append(unknown, user("orphan")).unwrap_err();
    assert!(matches!(error, Error::Rejected(Violation::UnknownParent(id)) if id == unknown));
    let error = log.append(root, started()).unwrap_err();
    assert!(matches!(
        error,
        Error::Rejected(Violation::MisplacedStart(_))
    ));

    assert_eq!(fs::metadata(&path).unwrap().len(), size);
    assert_eq!(log.events().len(), 1);
}

#[test]
fn second_open_is_locked_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let log = SessionLog::create(&path, started()).unwrap();

    assert!(matches!(SessionLog::open(&path), Err(Error::Locked(_))));
    drop(log);
    SessionLog::open(&path).unwrap();
}

#[test]
fn create_requires_a_new_file_and_a_start_event() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);

    let error = SessionLog::create(&path, user("hi")).unwrap_err();
    assert!(matches!(error, Error::Rejected(Violation::BadRoot)));
    assert!(!path.exists());

    drop(SessionLog::create(&path, started()).unwrap());
    assert!(matches!(
        SessionLog::create(&path, started()),
        Err(Error::Io { .. })
    ));
}

#[test]
fn context_and_taint_follow_the_branch() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(session_file(&dir), started()).unwrap();
    let root = log.root().id;
    let u1 = log.append(root, user("one")).unwrap().id;
    let tainted = log
        .append(
            u1,
            EventKind::Tainted {
                call: None,
                reason: "fetched a web page".into(),
            },
        )
        .unwrap()
        .id;
    let old = log
        .append(
            tainted,
            EventKind::Compaction {
                summary: "old".into(),
                tokens_before: 10,
            },
        )
        .unwrap()
        .id;
    let new = log
        .append(
            old,
            EventKind::Compaction {
                summary: "new".into(),
                tokens_before: 20,
            },
        )
        .unwrap()
        .id;
    let u2 = log.append(new, user("two")).unwrap().id;
    let sibling = log.append(u1, user("other branch")).unwrap().id;

    let context = log.context(u2).unwrap();
    assert_eq!(context.summary, Some("new"));
    let ids: Vec<_> = context.events.iter().map(|e| e.id).collect();
    assert_eq!(ids, [u2]);

    let context = log.context(sibling).unwrap();
    assert_eq!(context.summary, None);
    let ids: Vec<_> = context.events.iter().map(|e| e.id).collect();
    assert_eq!(ids, [root, u1, sibling]);

    assert_eq!(log.is_tainted(u2), Some(true));
    assert_eq!(log.is_tainted(sibling), Some(false));
    assert_eq!(log.is_tainted(EventId::generate()), None);
}
