use std::fmt::Write as _;
use std::fs;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::Value;
use xi_protocol::{OutputBlock, ToolCallId, ToolOutput};
use xxhash_rust::xxh3::xxh3_64;

use crate::{Context, Ran, output, parse, paths};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadInput {
    pub(crate) path: String,
    #[serde(default = "one")]
    offset: usize,
    #[serde(default = "read_limit")]
    limit: usize,
}
fn one() -> usize {
    1
}
fn read_limit() -> usize {
    2000
}
pub(crate) fn read_input(input: &Value) -> Result<ReadInput, String> {
    let input: ReadInput = parse(input)?;
    if input.offset == 0 || input.limit == 0 {
        return Err("offset and limit must be positive".into());
    }
    Ok(input)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteInput {
    pub(crate) path: String,
    content: String,
}
pub(crate) fn write_input(input: &Value) -> Result<WriteInput, String> {
    parse(input)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReplaceInput {
    pub(crate) path: String,
    old: String,
    new: String,
    #[serde(default)]
    all: bool,
}
pub(crate) fn replace_input(input: &Value) -> Result<ReplaceInput, String> {
    let input: ReplaceInput = parse(input)?;
    if input.old.is_empty() {
        return Err("old must not be empty".into());
    }
    Ok(input)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditInput {
    pub(crate) path: String,
    edits: Vec<Edit>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Edit {
    Range(Replacement),
    Insert(Insertion),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    start: String,
    end: Option<String>,
    lines: Vec<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Insertion {
    after: String,
    lines: Vec<String>,
}

#[derive(Clone, Copy)]
struct Anchor {
    line: usize,
    hash: u8,
}
impl Anchor {
    fn parse(anchor: &str) -> Result<Self, String> {
        let invalid = || format!("Invalid anchor {anchor:?}: expected LINE:HH from read or search");
        // Models often copy the whole `LINE:HH|content` row; the hash still
        // guards against stale content, so the copied text is ignored.
        let bare = anchor.split_once('|').map_or(anchor, |(bare, _)| bare);
        let (line, hash) = bare.split_once(':').ok_or_else(invalid)?;
        if line.is_empty()
            || !line.bytes().all(|c| c.is_ascii_digit())
            || hash.len() != 2
            || !hash.bytes().all(|c| c.is_ascii_hexdigit())
        {
            return Err(invalid());
        }
        let line = line.parse().map_err(|_| invalid())?;
        if line == 0 {
            return Err(invalid());
        }
        Ok(Self {
            line,
            hash: u8::from_str_radix(hash, 16).map_err(|_| invalid())?,
        })
    }
}

pub(crate) fn edit_input(input: &Value) -> Result<EditInput, String> {
    let input: EditInput = parse(input)?;
    if input.edits.is_empty() {
        return Err("edits must not be empty".into());
    }
    for edit in &input.edits {
        let lines = match edit {
            Edit::Range(edit) => {
                let start = Anchor::parse(&edit.start)?;
                let end = Anchor::parse(edit.end.as_deref().unwrap_or(&edit.start))?;
                if end.line < start.line {
                    return Err("end must not precede start".into());
                }
                &edit.lines
            }
            Edit::Insert(edit) => {
                if edit.after != "0" {
                    Anchor::parse(&edit.after)?;
                }
                &edit.lines
            }
        };
        if lines.iter().any(|line| line.contains(['\r', '\n'])) {
            return Err("Each lines item must be one line without CR or LF".into());
        }
    }
    Ok(input)
}

fn hash(text: &str) -> u8 {
    (xxh3_64(text.as_bytes()) >> 56) as u8
}

pub(crate) fn anchored(out: &mut String, number: usize, text: &str) {
    let end = text
        .char_indices()
        .nth(2000)
        .map_or(text.len(), |(index, _)| index);
    writeln!(
        out,
        "{number}:{:02x}|{}{}",
        hash(text),
        &text[..end],
        if end < text.len() {
            " [line truncated at 2000 chars]"
        } else {
            ""
        }
    )
    .expect("writing String");
}

#[derive(Clone, Copy)]
struct Line<'a> {
    text: &'a str,
    ending: &'a str,
}
fn lines(text: &str) -> Vec<Line<'_>> {
    text.split_inclusive('\n')
        .map(|line| {
            if let Some(text) = line.strip_suffix("\r\n") {
                Line {
                    text,
                    ending: "\r\n",
                }
            } else if let Some(text) = line.strip_suffix('\n') {
                Line { text, ending: "\n" }
            } else {
                Line {
                    text: line,
                    ending: "",
                }
            }
        })
        .collect()
}

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

pub(crate) fn read(call: &ToolCallId, input: &Value, ctx: &Context) -> Result<Ran, String> {
    let input = read_input(input)?;
    let path = paths::resolve(&input.path, ctx)?;
    let meta =
        fs::metadata(&path).map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
    if meta.is_dir() {
        let mut entries = fs::read_dir(&path)
            .map_err(|e| e.to_string())?
            .map(|entry| {
                let entry = entry?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.path().is_dir() {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| e.to_string())?;
        entries.sort();
        return Ok(output::text(call, entries.join("\n"), false, ctx));
    }
    if !meta.is_file() {
        return Err("Refusing to read a non-regular file".into());
    }
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    if let Some(media_type) = image_type(&bytes) {
        return Ok(Ran {
            output: ToolOutput {
                content: vec![OutputBlock::Image {
                    media_type: media_type.to_owned(),
                    data: base64::engine::general_purpose::STANDARD.encode(&bytes),
                }],
                is_error: false,
                untrusted: false,
            },
            spill: None,
        });
    }
    let text = std::str::from_utf8(&bytes)
        .ok()
        .filter(|_| !bytes.contains(&0))
        .ok_or_else(|| {
            format!(
                "Binary file: {} bytes. Refusing to display as text.",
                bytes.len()
            )
        })?;
    let mut out = String::new();
    let end = input.offset.saturating_add(input.limit);
    let mut truncated = false;
    for (index, line) in text
        .split_inclusive('\n')
        .enumerate()
        .skip(input.offset - 1)
    {
        let number = index + 1;
        if number >= end {
            truncated = true;
            break;
        }
        let line = if let Some(line) = line.strip_suffix('\n') {
            line.strip_suffix('\r').unwrap_or(line)
        } else {
            line
        };
        anchored(&mut out, number, line);
    }
    if truncated {
        writeln!(
            out,
            "[More lines. Continue with read {{\"path\":{},\"offset\":{end},\"limit\":{}}}.]",
            serde_json::to_string(&input.path).expect("serializing path"),
            input.limit
        )
        .expect("writing String");
    }
    Ok(output::text(call, out, false, ctx))
}

pub(crate) fn write(call: &ToolCallId, input: &Value, ctx: &Context) -> Result<Ran, String> {
    let input = write_input(input)?;
    let path = paths::resolve(&input.path, ctx)?;
    check_cancel(ctx)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    check_cancel(ctx)?;
    fs::write(&path, &input.content).map_err(|e| e.to_string())?;
    Ok(output::text(
        call,
        format!(
            "Wrote {} bytes, {} lines to {}.",
            input.content.len(),
            input.content.lines().count(),
            input.path
        ),
        false,
        ctx,
    ))
}

pub(crate) fn replace(call: &ToolCallId, input: &Value, ctx: &Context) -> Result<Ran, String> {
    let input = replace_input(input)?;
    let path = paths::resolve(&input.path, ctx)?;
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let count = text.matches(&input.old).count();
    if count == 0 || (!input.all && count != 1) {
        return Err(format!(
            "old occurs {count} times; expected {}. No changes written.",
            if input.all {
                "at least once"
            } else {
                "exactly once"
            }
        ));
    }
    let changed = text.replace(&input.old, &input.new);
    check_cancel(ctx)?;
    fs::write(&path, changed).map_err(|e| e.to_string())?;
    Ok(output::text(
        call,
        format!("Replaced {count} occurrence(s) in {}.", input.path),
        false,
        ctx,
    ))
}

struct Operation<'a> {
    start: usize,
    end: usize,
    lines: &'a [String],
}

pub(crate) fn edit(call: &ToolCallId, input: &Value, ctx: &Context) -> Result<Ran, String> {
    let input = edit_input(input)?;
    let path = paths::resolve(&input.path, ctx)?;
    let original = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let before = lines(&original);
    let mut mismatches = Vec::new();
    let mut check = |anchor: &str| -> Result<usize, String> {
        let anchor = Anchor::parse(anchor)?;
        if before
            .get(anchor.line - 1)
            .is_none_or(|line| hash(line.text) != anchor.hash)
        {
            mismatches.push(anchor.line);
        }
        Ok(anchor.line)
    };
    let mut operations = Vec::with_capacity(input.edits.len());
    for edit in &input.edits {
        operations.push(match edit {
            Edit::Range(edit) => Operation {
                start: check(&edit.start)? - 1,
                end: check(edit.end.as_deref().unwrap_or(&edit.start))?,
                lines: &edit.lines,
            },
            Edit::Insert(edit) => {
                let at = if edit.after == "0" {
                    0
                } else {
                    check(&edit.after)?
                };
                Operation {
                    start: at,
                    end: at,
                    lines: &edit.lines,
                }
            }
        });
    }
    if !mismatches.is_empty() {
        mismatches.sort_unstable();
        mismatches.dedup();
        let mut message = "Stale anchors. No changes written. Fresh context:\n".to_owned();
        for line in mismatches {
            writeln!(message, "[anchor at line {line} did not match]").expect("writing String");
            let center = line.saturating_sub(1).min(before.len().saturating_sub(1));
            context(
                &mut message,
                &before,
                center.saturating_sub(2),
                (center + 3).min(before.len()),
            );
        }
        return Err(message);
    }
    operations.sort_by_key(|op| (op.start, op.end));
    for pair in operations.windows(2) {
        if pair[0].end > pair[1].start
            || (pair[0].start == pair[0].end
                && pair[0].start == pair[1].start
                && pair[1].start == pair[1].end)
        {
            return Err("Overlapping edits. No changes written.".into());
        }
    }
    let newline = before
        .iter()
        .find(|line| !line.ending.is_empty())
        .map_or("\n", |line| line.ending);
    let mut after = Vec::new();
    let mut cursor = 0;
    let mut regions = Vec::new();
    for operation in operations {
        after.extend_from_slice(&before[cursor..operation.start]);
        let start = after.len();
        after.extend(operation.lines.iter().map(|text| Line {
            text,
            ending: newline,
        }));
        regions.push((start, after.len()));
        cursor = operation.end;
    }
    after.extend_from_slice(&before[cursor..]);
    let mut changed = String::with_capacity(original.len());
    for (index, line) in after.iter().enumerate() {
        changed.push_str(line.text);
        if index + 1 < after.len() || original.ends_with('\n') {
            changed.push_str(if line.ending.is_empty() {
                newline
            } else {
                line.ending
            });
        }
    }
    check_cancel(ctx)?;
    fs::write(&path, changed).map_err(|e| e.to_string())?;
    let mut out = format!("Edited {}.\n", input.path);
    let mut shown = 0;
    for (start, end) in regions {
        let start = start.saturating_sub(2).max(shown);
        let end = end.saturating_add(2).min(after.len());
        if start >= end {
            continue;
        }
        if shown > 0 && start > shown {
            out.push_str("...\n");
        }
        context(&mut out, &after, start, end);
        shown = end;
    }
    if after.is_empty() {
        out.push_str("[empty file]\n");
    }
    Ok(output::text(call, out, false, ctx))
}

fn context(out: &mut String, lines: &[Line<'_>], start: usize, end: usize) {
    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        anchored(out, index + 1, line.text);
    }
}

fn check_cancel(ctx: &Context) -> Result<(), String> {
    if ctx.cancel.is_cancelled() {
        Err("Tool cancelled; no changes written".into())
    } else {
        Ok(())
    }
}
