//! Blocking external-editor handoff for composer drafts.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::Command;

/// Edits a temporary copy of `text` with `$VISUAL`, then `$EDITOR`, then `vi`.
/// The editor command is interpreted by `sh`, with the file passed separately
/// as `$1`. A zero exit returns its contents without one trailing newline;
/// any other exit returns `None`. The caller must suspend terminal raw mode.
pub fn edit(text: &str) -> io::Result<Option<String>> {
    let editor = env::var_os("VISUAL")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("EDITOR").filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "vi".into());
    let mut file = tempfile::Builder::new()
        .prefix("xi-prompt-")
        .suffix(".txt")
        .tempfile()?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    let mut script = editor;
    script.push(" \"$1\"");
    let status = Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("xi")
        .arg(file.path())
        .status()?;
    if !status.success() {
        return Ok(None);
    }
    let mut edited = fs::read_to_string(file.path())?;
    if edited.ends_with('\n') {
        edited.pop();
        if edited.ends_with('\r') {
            edited.pop();
        }
    }
    Ok(Some(edited))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Only child processes receive editor environment changes. This also tests
    // quoted commands and file paths containing spaces without unsafe set_var.
    #[test]
    fn edit_uses_visual_and_preserves_draft_on_nonzero_exit() {
        const CHILD: &str = "XI_TUI_EXTERNAL_TEST_CHILD";
        if let Ok(case) = env::var(CHILD) {
            let result = edit("original ξ\nsecond line").unwrap();
            match case.as_str() {
                "visual" | "editor" => assert_eq!(result.as_deref(), Some("edited ξ\n")),
                "nonzero" => assert_eq!(result, None),
                "unchanged" => assert_eq!(result.as_deref(), Some("original ξ\nsecond line")),
                "crlf" => assert_eq!(result.as_deref(), Some("edited")),
                _ => panic!("unknown child case"),
            }
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake editor.sh");
        fs::write(
            &script,
            concat!(
                "[ \"$(cat \"$2\")\" = 'original ξ\nsecond line' ] || exit 9\n",
                "case \"$1\" in\n",
                "  edit) printf 'edited ξ\\n\\n' > \"$2\" ;;\n",
                "  fail) printf 'discarded' > \"$2\"; exit 1 ;;\n",
                "  unchanged) exit 0 ;;\n",
                "  crlf) printf 'edited\\r\\n' > \"$2\" ;;\n",
                "esac\n",
            ),
        )
        .unwrap();
        for (case, visual, editor) in [
            ("visual", "edit", "fail"),
            ("editor", "", "edit"),
            ("nonzero", "fail", "edit"),
            ("unchanged", "unchanged", "fail"),
            ("crlf", "crlf", "fail"),
        ] {
            let editor_command = |mode: &str| format!("sh '{}' {mode}", script.display());
            let output = Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "external::tests::edit_uses_visual_and_preserves_draft_on_nonzero_exit",
                    "--nocapture",
                ])
                .env(CHILD, case)
                .env(
                    "VISUAL",
                    if visual.is_empty() {
                        String::new()
                    } else {
                        editor_command(visual)
                    },
                )
                .env("EDITOR", editor_command(editor))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{case}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
