//! Blocking external-editor handoff for composer drafts and config files.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitStatus};

/// Edits a temporary copy of `text` with `$VISUAL`, then `$EDITOR`, then `vi`.
/// The editor command is interpreted by `sh`, with the file passed separately
/// as `$1`. A zero exit returns its contents without one trailing newline;
/// any other exit returns `None`. The caller must suspend terminal raw mode.
pub fn edit(text: &str) -> io::Result<Option<String>> {
    let mut file = tempfile::Builder::new()
        .prefix("xi-prompt-")
        .suffix(".txt")
        .tempfile()?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    let status = run(file.path())?;
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

/// Opens a file with `$VISUAL`, then `$EDITOR`, then `vi`.
///
/// The file path is passed separately to `sh`, and missing parent directories
/// are created. The caller must suspend terminal raw mode.
pub fn edit_file(path: &Path) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let status = run(path)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("editor exited with {status}")))
    }
}

fn run(path: &Path) -> io::Result<ExitStatus> {
    let mut script = env::var_os("VISUAL")
        .filter(|value| !value.is_empty())
        .or_else(|| env::var_os("EDITOR").filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "vi".into());
    script.push(" \"$1\"");
    Command::new("sh")
        .arg("-c")
        .arg(script)
        .arg("xi")
        .arg(path)
        .status()
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

    #[test]
    fn edit_file_passes_the_exact_path_and_creates_its_parent() {
        const CHILD: &str = "XI_TUI_EXTERNAL_FILE_TEST_CHILD";
        if let Some(path) = env::var_os(CHILD) {
            edit_file(Path::new(&path)).unwrap();
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new config directory/config file.toml");
        let script = dir.path().join("fake file editor.sh");
        fs::write(
            &script,
            "printf '# edited\\napprovals = \"reads\"\\n' > \"$1\"\n",
        )
        .unwrap();
        let output = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "external::tests::edit_file_passes_the_exact_path_and_creates_its_parent",
                "--nocapture",
            ])
            .env(CHILD, &path)
            .env("VISUAL", format!("sh '{}'", script.display()))
            .env("EDITOR", "exit 9")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "# edited\napprovals = \"reads\"\n"
        );
    }
}
