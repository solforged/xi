use std::fs;
use std::path::PathBuf;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use xi_protocol::{OutputBlock, ToolCallId};
use xi_sandbox::Spec;

use crate::{Context, Crossing, Ran, by_name, truncate};

struct Fixture {
    _root: tempfile::TempDir,
    ctx: Context,
    outside: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let workspace = root_path.join("workspace");
        let tmp = root_path.join("tmp");
        let outside = root_path.join("outside");
        for path in [&workspace, &tmp, &outside] {
            fs::create_dir(path).unwrap();
        }
        fs::create_dir(workspace.join(".git")).unwrap();
        let sandbox = Spec::new(&workspace, &tmp);
        let helper = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("xi-sandbox-exec");
        Self {
            _root: root,
            outside,
            ctx: Context {
                workspace,
                sandbox,
                helper,
                spill_dir: tmp.join("spill"),
                output_limit: 64 * 1024,
                cancel: CancellationToken::new(),
            },
        }
    }
    fn put(&self, path: &str, contents: &str) {
        fs::write(self.ctx.workspace.join(path), contents).unwrap();
    }
    fn get(&self, path: &str) -> String {
        fs::read_to_string(self.ctx.workspace.join(path)).unwrap()
    }
    async fn run(&self, name: &str, input: Value) -> Ran {
        by_name(name)
            .unwrap()
            .run(&ToolCallId::from("test-call"), input, &self.ctx)
            .await
    }
    fn crossings(&self, name: &str, input: Value) -> Result<Vec<Crossing>, String> {
        by_name(name).unwrap().crossings(&input, &self.ctx)
    }
}
fn text(ran: &Ran) -> &str {
    match &ran.output.content[0] {
        OutputBlock::Text { text } => text,
        _ => panic!("expected text"),
    }
}
fn anchor(line: usize, content: &str) -> String {
    format!(
        "{line}:{:02x}",
        xxhash_rust::xxh3::xxh3_64(content.as_bytes()) >> 56
    )
}

#[tokio::test]
async fn read_anchors_offsets_and_clipping() {
    let f = Fixture::new();
    f.put("text", "first\r\nsecond\r\nthird\r\n");
    let ran = f
        .run("read", json!({"path":"text", "offset":2, "limit":1}))
        .await;
    assert!(!ran.output.is_error);
    assert!(text(&ran).starts_with(&format!("{}|second\n", anchor(2, "second"))));
    assert!(text(&ran).contains("\"offset\":3"));
    let long = "é".repeat(2001);
    f.put("long", &long);
    let ran = f.run("read", json!({"path":"long"})).await;
    assert!(text(&ran).starts_with(&format!("{}|", anchor(1, &long))));
    assert_eq!(text(&ran).matches('é').count(), 2000);
    assert!(text(&ran).contains("line truncated"));
}

#[tokio::test]
async fn read_defaults_to_two_thousand_lines() {
    let f = Fixture::new();
    f.put("text", &"x\n".repeat(2001));
    let ran = f.run("read", json!({"path":"text"})).await;
    assert_eq!(
        text(&ran).lines().filter(|line| line.contains('|')).count(),
        2000
    );
    assert!(text(&ran).contains("\"offset\":2001"));
}

#[tokio::test]
async fn read_directory_images_and_binary() {
    use base64::Engine as _;
    let f = Fixture::new();
    fs::create_dir(f.ctx.workspace.join("adir")).unwrap();
    f.put("z", "z");
    let ran = f.run("read", json!({"path":"."})).await;
    assert_eq!(text(&ran), ".git/\nadir/\nz");
    for (mime, bytes) in [
        ("image/png", b"\x89PNG\r\n\x1a\n".as_slice()),
        ("image/jpeg", b"\xff\xd8\xff"),
        ("image/gif", b"GIF89a"),
        ("image/webp", b"RIFF1234WEBP"),
    ] {
        fs::write(f.ctx.workspace.join("image"), bytes).unwrap();
        let ran = f.run("read", json!({"path":"image"})).await;
        assert_eq!(
            ran.output.content,
            vec![OutputBlock::Image {
                media_type: mime.into(),
                data: base64::engine::general_purpose::STANDARD.encode(bytes)
            }]
        );
    }
    fs::write(f.ctx.workspace.join("binary"), [0, 1, 2, 255]).unwrap();
    let ran = f.run("read", json!({"path":"binary"})).await;
    assert!(ran.output.is_error);
    assert!(text(&ran).contains("4 bytes"));
}

#[tokio::test]
async fn edit_replaces_inserts_and_deletes_from_original_snapshot() {
    let f = Fixture::new();
    f.put("text", "a\nb\nc\nd\ne\n");
    let ran = f
        .run(
            "edit",
            json!({"path":"text", "edits":[
                {"start":anchor(4,"d"), "end":anchor(5,"e"), "lines":[]},
                {"after":"0", "lines":["top"]},
                {"start":anchor(2,"b"), "lines":["B","BB"]},
                {"after":anchor(3,"c"), "lines":["inserted"]}
            ]}),
        )
        .await;
    assert!(!ran.output.is_error, "{}", text(&ran));
    assert_eq!(f.get("text"), "top\na\nB\nBB\nc\ninserted\n");
    assert!(text(&ran).contains(&format!("{}|BB", anchor(4, "BB"))));
}

#[tokio::test]
async fn edit_stale_anchors_reject_every_change_and_show_each_context() {
    let f = Fixture::new();
    let original = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
    f.put("text", original);
    let ran = f
        .run(
            "edit",
            json!({"path":"text", "edits":[
                {"after":"0", "lines":["must not write"]},
                {"start":anchor(2,"old b"), "lines":["bad"]},
                {"start":anchor(9,"old i"), "lines":["bad"]}
            ]}),
        )
        .await;
    assert!(ran.output.is_error);
    assert_eq!(f.get("text"), original);
    assert!(text(&ran).contains(&format!("{}|b", anchor(2, "b"))));
    assert!(text(&ran).contains(&format!("{}|i", anchor(9, "i"))));
}

#[tokio::test]
async fn edit_rejects_overlapping_ranges_and_interior_insertions() {
    let f = Fixture::new();
    let original = "a\nb\nc\nd\n";
    for edits in [
        json!([{"start":anchor(1,"a"),"end":anchor(3,"c"),"lines":[]},{"start":anchor(2,"b"),"lines":[]}]),
        json!([{"start":anchor(1,"a"),"end":anchor(3,"c"),"lines":[]},{"after":anchor(2,"b"),"lines":["x"]}]),
        json!([{"after":"0","lines":["x"]},{"after":"0","lines":["y"]}]),
    ] {
        f.put("text", original);
        let ran = f.run("edit", json!({"path":"text", "edits":edits})).await;
        assert!(ran.output.is_error);
        assert!(text(&ran).contains("Overlapping"));
        assert_eq!(f.get("text"), original);
    }
}

#[tokio::test]
async fn edit_preserves_crlf_and_trailing_newline_presence() {
    let f = Fixture::new();
    for (original, expected) in [
        ("a\r\nb\r\n", "a\r\nB\r\nC\r\n"),
        ("a\r\nb", "a\r\nB\r\nC"),
        ("a\nb", "a\nB\nC"),
    ] {
        f.put("text", original);
        let ran = f
            .run(
                "edit",
                json!({"path":"text","edits":[{"start":anchor(2,"b"),"lines":["B","C"]}]}),
            )
            .await;
        assert!(!ran.output.is_error);
        assert_eq!(f.get("text"), expected);
    }
    f.put("text", "");
    let ran = f
        .run(
            "edit",
            json!({"path":"text","edits":[{"after":"0","lines":["first"]}]}),
        )
        .await;
    assert!(!ran.output.is_error);
    assert_eq!(f.get("text"), "first");
    let ran = f
        .run(
            "edit",
            json!({"path":"text","edits":[{"start":anchor(1,"first"),"lines":[]}]}),
        )
        .await;
    assert!(!ran.output.is_error);
    assert_eq!(f.get("text"), "");
}

#[tokio::test]
async fn edit_keeps_unmodified_mixed_line_endings() {
    let f = Fixture::new();
    f.put("text", "a\r\nb\nc\r\n");
    let ran = f
        .run(
            "edit",
            json!({"path":"text","edits":[{"after":anchor(3,"c"),"lines":["d"]}]}),
        )
        .await;
    assert!(!ran.output.is_error);
    assert_eq!(f.get("text"), "a\r\nb\nc\r\nd\r\n");
}

#[tokio::test]
async fn replace_requires_unique_text_unless_all() {
    let f = Fixture::new();
    f.put("text", "a a b");
    for (old, count) in [("a", 2), ("missing", 0)] {
        let ran = f
            .run("replace", json!({"path":"text","old":old,"new":"x"}))
            .await;
        assert!(ran.output.is_error);
        assert!(text(&ran).contains(&format!("{count} times")));
        assert_eq!(f.get("text"), "a a b");
    }
    assert!(
        !f.run("replace", json!({"path":"text","old":"b","new":"B"}))
            .await
            .output
            .is_error
    );
    assert!(
        !f.run(
            "replace",
            json!({"path":"text","old":"a","new":"A","all":true})
        )
        .await
        .output
        .is_error
    );
    assert_eq!(f.get("text"), "A A B");
}

#[tokio::test]
async fn write_makes_parent_directories_and_reports_counts() {
    let f = Fixture::new();
    let ran = f
        .run(
            "write",
            json!({"path":"new/nested/text","content":"é\nline\n"}),
        )
        .await;
    assert!(!ran.output.is_error);
    assert!(text(&ran).contains("8 bytes, 2 lines"));
    assert_eq!(f.get("new/nested/text"), "é\nline\n");
}

#[test]
fn crossings_classify_read_write_and_protected_paths() {
    let mut f = Fixture::new();
    assert_eq!(
        f.crossings("read", json!({"path":"missing"})).unwrap(),
        vec![]
    );
    assert_eq!(
        f.crossings("write", json!({"path":"new/file","content":""}))
            .unwrap(),
        vec![]
    );
    assert_eq!(
        f.crossings(
            "write",
            json!({"path":".git/hooks/pre-commit","content":""})
        )
        .unwrap(),
        vec![Crossing::Write(
            f.ctx.workspace.join(".git/hooks/pre-commit")
        )]
    );
    let outside = f.outside.join("secret");
    assert_eq!(
        f.crossings("read", json!({"path":outside})).unwrap(),
        vec![Crossing::Read(outside.clone())]
    );
    f.ctx.sandbox.readable.push(f.outside.clone());
    assert_eq!(
        f.crossings("read", json!({"path":outside})).unwrap(),
        vec![]
    );
    assert_eq!(
        f.crossings("write", json!({"path":outside,"content":""}))
            .unwrap(),
        vec![Crossing::Write(outside)]
    );
    assert_eq!(
        f.crossings("bash", json!({"command":"echo ok"})).unwrap(),
        vec![Crossing::Exec("echo ok".into())]
    );
}

#[cfg(unix)]
#[test]
fn symlinks_and_new_descendants_are_resolved_before_crossings() {
    use std::os::unix::fs::symlink;
    let mut f = Fixture::new();
    symlink(&f.outside, f.ctx.workspace.join("link")).unwrap();
    fs::write(f.outside.join("secret"), "secret").unwrap();
    assert_eq!(
        f.crossings("read", json!({"path":"link/secret"})).unwrap(),
        vec![Crossing::Read(f.outside.join("secret"))]
    );
    assert_eq!(
        f.crossings("write", json!({"path":"link/new/deep","content":""}))
            .unwrap(),
        vec![Crossing::Write(f.outside.join("new/deep"))]
    );
    symlink(f.outside.join("absent"), f.ctx.workspace.join("dangling")).unwrap();
    assert_eq!(
        f.crossings("write", json!({"path":"dangling/file","content":""}))
            .unwrap(),
        vec![Crossing::Write(f.outside.join("absent/file"))]
    );
    assert_eq!(
        f.crossings("read", json!({"path":"link/../outside/secret"}))
            .unwrap(),
        vec![Crossing::Read(f.outside.join("secret"))]
    );
    f.ctx.sandbox.readable.push(f.ctx.workspace.join("link"));
    assert_eq!(
        f.crossings("read", json!({"path":f.outside.join("secret")}))
            .unwrap(),
        vec![]
    );
}

#[test]
fn invalid_input_is_rejected_before_policy() {
    let f = Fixture::new();
    for (tool, input) in [
        ("read", json!({})),
        ("write", json!({"path":"x"})),
        ("read", json!({"path":"x","offset":0})),
        ("read", json!({"path":""})),
        (
            "edit",
            json!({"path":"x","edits":[{"start":"1:xyz","lines":[]}]}),
        ),
        (
            "edit",
            json!({"path":"x","edits":[{"start":"0:ab","lines":[]}]}),
        ),
        (
            "edit",
            json!({"path":"x","edits":[{"after":"0","lines":["two\nlines"]}]}),
        ),
        ("replace", json!({"path":"x","old":"","new":"a"})),
        ("search", json!({})),
        ("search", json!({"pattern":"["})),
        ("bash", json!({"command":"x","timeout":601})),
    ] {
        assert!(f.crossings(tool, input).is_err(), "{tool}");
    }
}

#[tokio::test]
async fn search_respects_gitignore_hidden_files_and_match_limit() {
    let f = Fixture::new();
    f.put(".gitignore", "ignored\n");
    f.put("ignored", "needle\n");
    f.put(".hidden", "needle\n");
    f.put("a", "needle\nNEEDLE\nneedle\n");
    f.put("b", "needle\n");
    let ran = f
        .run(
            "search",
            json!({"pattern":"needle", "ignore_case":true,"limit":2}),
        )
        .await;
    assert!(!ran.output.is_error, "{}", text(&ran));
    assert!(text(&ran).starts_with(&format!(
        "a\n{}|needle\n{}|NEEDLE\n",
        anchor(1, "needle"),
        anchor(2, "NEEDLE")
    )));
    assert!(text(&ran).contains("Truncated after 2"));
    assert!(!text(&ran).contains("ignored"));
    let ran = f.run("search", json!({"glob":"*"})).await;
    assert_eq!(text(&ran), "a\nb\n");
    let ran = f.run("search", json!({"glob":"*","limit":1})).await;
    assert!(text(&ran).contains("Truncated after 1 files"));
}

#[tokio::test]
async fn search_outputs_editable_anchors_and_relative_outside_paths() {
    let f = Fixture::new();
    f.put("text", "before\r\nneedle\r\n");
    let ran = f
        .run("search", json!({"pattern":"needle","glob":"text"}))
        .await;
    let found = text(&ran)
        .lines()
        .nth(1)
        .unwrap()
        .split('|')
        .next()
        .unwrap();
    let ran = f
        .run(
            "edit",
            json!({"path":"text","edits":[{"start":found,"lines":["changed"]}]}),
        )
        .await;
    assert!(!ran.output.is_error);
    assert_eq!(f.get("text"), "before\r\nchanged\r\n");
    fs::write(f.outside.join("external"), "needle\n").unwrap();
    let ran = f
        .run("search", json!({"pattern":"needle","path":f.outside}))
        .await;
    assert!(text(&ran).starts_with("../outside/external\n"));
}

#[cfg(unix)]
#[tokio::test]
async fn search_does_not_follow_unapproved_symlink_descendants() {
    let f = Fixture::new();
    fs::write(f.outside.join("secret"), "needle\n").unwrap();
    std::os::unix::fs::symlink(&f.outside, f.ctx.workspace.join("link")).unwrap();
    let ran = f.run("search", json!({"pattern":"needle"})).await;
    assert_eq!(text(&ran), "");
}

#[test]
fn truncate_spills_complete_utf8_with_head_tail_and_safe_call_ids() {
    let mut f = Fixture::new();
    f.ctx.output_limit = 1000;
    let original = format!("HEAD{}TAIL", "é🙂".repeat(1000));
    let (short, spill) = truncate(&ToolCallId::from("../escape"), original.clone(), &f.ctx);
    let spill = spill.unwrap();
    assert_eq!(spill.parent().unwrap(), f.ctx.spill_dir);
    assert_eq!(fs::read_to_string(&spill).unwrap(), original);
    assert!(short.starts_with("HEAD"));
    assert!(short.ends_with("TAIL"));
    assert!(short.len() <= f.ctx.output_limit);
    assert!(short.contains("Full output:"));
    assert!(short.contains("\"offset\":1"));
    assert_eq!(
        truncate(&ToolCallId::from("small"), "short".into(), &f.ctx),
        ("short".into(), None)
    );
}

#[test]
fn truncate_reports_spill_write_failure_without_claiming_a_file() {
    let mut f = Fixture::new();
    f.ctx.output_limit = 1;
    fs::write(&f.ctx.spill_dir, "not a directory").unwrap();
    let (short, spill) = truncate(&ToolCallId::from("test"), "long text".into(), &f.ctx);
    assert!(spill.is_none());
    assert!(short.contains("could not write spill file"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell {
    use super::*;
    use std::time::Duration;

    async fn wait_for_file(path: &std::path::Path) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !path.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("child did not start");
    }
    async fn assert_dead(pid: i32) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err() {
                    break;
                }
                let status = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .unwrap();
                if String::from_utf8_lossy(&status.stdout)
                    .trim_start()
                    .starts_with('Z')
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("background child survived");
    }

    #[tokio::test]
    async fn bash_merges_stdout_stderr_in_order_and_reports_nonzero() {
        let f = Fixture::new();
        assert!(
            f.ctx.helper.is_file(),
            "build xi-sandbox before bash tests: {}",
            f.ctx.helper.display()
        );
        let ran = f
            .run(
                "bash",
                json!({"command":"printf one; printf two >&2; printf three; exit 7"}),
            )
            .await;
        assert!(ran.output.is_error);
        assert_eq!(text(&ran), "onetwothree\n[exit 7]");
        let ran = f
            .run(
                "bash",
                json!({"command":"pwd; if read line; then exit 9; fi"}),
            )
            .await;
        assert!(!ran.output.is_error, "{}", text(&ran));
        assert_eq!(
            text(&ran),
            format!("{}\n\n[exit 0]", f.ctx.workspace.display())
        );
    }

    #[tokio::test]
    async fn bash_timeout_kills_background_process_group() {
        let f = Fixture::new();
        let ran = f
            .run(
                "bash",
                json!({"command":"sleep 60 & echo $! > child; wait", "timeout":1}),
            )
            .await;
        assert!(ran.output.is_error);
        assert!(
            text(&ran).ends_with("[timed out after 1s]"),
            "{}",
            text(&ran)
        );
        assert_dead(f.get("child").trim().parse().unwrap()).await;
    }

    #[tokio::test]
    async fn bash_cancel_kills_background_process_group() {
        let f = Fixture::new();
        let child = f.ctx.workspace.join("child");
        let cancel = async {
            wait_for_file(&child).await;
            f.ctx.cancel.cancel();
        };
        let (ran, ()) = tokio::join!(
            f.run(
                "bash",
                json!({"command":"sleep 60 & echo $! > child; wait"})
            ),
            cancel
        );
        assert!(ran.output.is_error);
        assert!(text(&ran).ends_with("[cancelled]"));
        assert_dead(f.get("child").trim().parse().unwrap()).await;
    }

    #[tokio::test]
    async fn bash_future_drop_kills_background_process_group() {
        let f = Fixture::new();
        let ctx = f.ctx.clone();
        let task = tokio::spawn(async move {
            by_name("bash")
                .unwrap()
                .run(
                    &ToolCallId::from("drop"),
                    json!({"command":"sleep 60 & echo $! > child; wait"}),
                    &ctx,
                )
                .await
        });
        wait_for_file(&f.ctx.workspace.join("child")).await;
        let pid = f.get("child").trim().parse().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_dead(pid).await;
    }

    #[tokio::test]
    async fn bash_spills_large_output_and_preserves_both_ends() {
        let mut f = Fixture::new();
        f.ctx.output_limit = 1024;
        let ran = f.run("bash",json!({"command":"printf HEAD; i=0; while [ $i -lt 20000 ]; do printf 'abcdefghij'; i=$((i+1)); done; printf TAIL"})).await;
        assert!(!ran.output.is_error, "{}", text(&ran));
        assert!(text(&ran).starts_with("HEAD"));
        assert!(text(&ran).ends_with("TAIL\n[exit 0]"));
        assert!(text(&ran).len() <= 1024);
        let full = fs::read_to_string(ran.spill.unwrap()).unwrap();
        assert_eq!(
            full,
            format!("HEAD{}TAIL\n[exit 0]", "abcdefghij".repeat(20000))
        );
    }

    #[tokio::test]
    async fn bash_helper_failure_is_explicit() {
        let mut f = Fixture::new();
        f.ctx.sandbox.cwd = f.outside.join("missing");
        // A readable root that is a file is valid, but a protected path below
        // a regular file makes sandbox setup fail before user code can run.
        f.put("file", "not a directory");
        f.ctx
            .sandbox
            .protected
            .push(f.ctx.workspace.join("file/impossible"));
        let ran = f.run("bash", json!({"command":"echo MUST_NOT_RUN"})).await;
        assert!(ran.output.is_error);
        assert!(
            text(&ran).contains("Sandbox could not start"),
            "{}",
            text(&ran)
        );
        assert!(text(&ran).contains("125"));
        assert!(!text(&ran).contains("MUST_NOT_RUN"));
    }
}

#[tokio::test]
async fn search_keeps_bom_and_lone_carriage_return_in_anchors() {
    let f = Fixture::new();
    for contents in ["\u{feff}needle\n", "needle\r"] {
        f.put("text", contents);
        let read = f.run("read", json!({"path":"text"})).await;
        let search = f
            .run("search", json!({"pattern":"needle","path":"text"}))
            .await;
        assert_eq!(text(&search), format!("text\n{}", text(&read)));
    }
    fs::write(f.ctx.workspace.join("text"), [0xff, 0xfe, b'n', 0, b'e', 0]).unwrap();
    let search = f.run("search", json!({"pattern":"ne","path":"text"})).await;
    assert_eq!(text(&search), "");
}

#[tokio::test]
async fn cancelled_file_mutations_do_not_write() {
    let f = Fixture::new();
    f.put("text", "original\n");
    f.ctx.cancel.cancel();
    for (name, input) in [
        ("write", json!({"path":"new/file","content":"bad"})),
        (
            "replace",
            json!({"path":"text","old":"original","new":"bad"}),
        ),
        (
            "edit",
            json!({"path":"text","edits":[{"start":anchor(1,"original"),"lines":["bad"]}]}),
        ),
    ] {
        let ran = f.run(name, input).await;
        assert!(ran.output.is_error);
        assert!(text(&ran).contains("cancelled"));
    }
    assert_eq!(f.get("text"), "original\n");
    assert!(!f.ctx.workspace.join("new").exists());
}
