use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::PathBuf;

use xi_protocol::{OutputBlock, ToolCallId, ToolOutput};

use crate::{Context, Ran};

pub(crate) fn text(call: &ToolCallId, text: String, is_error: bool, ctx: &Context) -> Ran {
    let (text, spill) = truncate(call, text, ctx);
    Ran {
        output: ToolOutput {
            content: vec![OutputBlock::Text { text }],
            is_error,
            untrusted: false,
        },
        spill,
    }
}

fn spill_path(call: &ToolCallId, ctx: &Context) -> PathBuf {
    // Provider ids are opaque, not trusted filenames. Keep ordinary ids intact.
    let mut name = String::new();
    for byte in call.0.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
            name.push(char::from(byte));
        } else {
            write!(name, "%{byte:02X}").expect("writing a String");
        }
    }
    if name.is_empty() {
        name.push_str("%00");
    }
    ctx.spill_dir.join(format!("{name}.txt"))
}

fn marker(path: &std::path::Path) -> String {
    format!(
        "\n[Output truncated. Full output: {}. Read it with read {{\"path\":{},\"offset\":1,\"limit\":2000}}.]\n",
        path.display(),
        serde_json::to_string(&path.to_string_lossy()).expect("serializing path")
    )
}

fn clipped(head: &str, tail: &str, limit: usize, marker: &str) -> String {
    // The locator must remain readable even when the configured budget is
    // smaller than the locator itself. In that case no payload is retained.
    let budget = limit.saturating_sub(marker.len());
    let mut end = head.len().min(budget.div_ceil(2));
    while !head.is_char_boundary(end) {
        end -= 1;
    }
    let mut start = tail.len().saturating_sub(budget / 2);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    format!("{}{marker}{}", &head[..end], &tail[start..])
}

pub(crate) fn truncate(
    call: &ToolCallId,
    text: String,
    ctx: &Context,
) -> (String, Option<PathBuf>) {
    if text.len() <= ctx.output_limit {
        return (text, None);
    }
    let path = spill_path(call, ctx);
    match fs::create_dir_all(&ctx.spill_dir).and_then(|()| fs::write(&path, &text)) {
        Ok(()) => (
            clipped(&text, &text, ctx.output_limit, &marker(&path)),
            Some(path),
        ),
        Err(error) => {
            let notice = format!(
                "\n[Output truncated; could not write spill file {}: {error}]\n",
                path.display()
            );
            (clipped(&text, &text, ctx.output_limit, &notice), None)
        }
    }
}

// Before spilling, head holds the complete output. After spilling, only a
// bounded prefix and ring-buffer suffix remain. Writes stream straight to disk.
pub(crate) struct Capture {
    limit: usize,
    path: PathBuf,
    file: Option<File>,
    head: Vec<u8>,
    tail: VecDeque<u8>,
}

impl Capture {
    pub(crate) fn new(call: &ToolCallId, ctx: &Context) -> Self {
        Self {
            limit: ctx.output_limit,
            path: spill_path(call, ctx),
            file: None,
            head: Vec::new(),
            tail: VecDeque::new(),
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.file.is_none() && bytes.len() <= self.limit.saturating_sub(self.head.len()) {
            self.head.extend_from_slice(bytes);
            return Ok(());
        }
        if self.file.is_none() {
            fs::create_dir_all(self.path.parent().expect("spill directory"))?;
            let mut file = File::create(&self.path)?;
            file.write_all(&self.head)?;
            let start = self.head.len().saturating_sub(self.limit / 2 + 4);
            self.tail.extend(&self.head[start..]);
            self.head.truncate(self.limit.div_ceil(2).saturating_add(4));
            self.file = Some(file);
        }
        self.file.as_mut().expect("spill opened").write_all(bytes)?;
        let head_limit = self.limit.div_ceil(2).saturating_add(4);
        let take = bytes.len().min(head_limit.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..take]);
        let capacity = self.limit / 2 + 4;
        if bytes.len() >= capacity {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - capacity..]);
        } else {
            let discard = (self.tail.len() + bytes.len()).saturating_sub(capacity);
            self.tail.drain(..discard);
            self.tail.extend(bytes);
        }
        Ok(())
    }

    pub(crate) fn finish(self, is_error: bool) -> Ran {
        let (text, spill) = if self.file.is_some() {
            let head = String::from_utf8_lossy(&self.head);
            let tail: Vec<_> = self.tail.into_iter().collect();
            let tail = String::from_utf8_lossy(&tail);
            (
                clipped(&head, &tail, self.limit, &marker(&self.path)),
                Some(self.path),
            )
        } else {
            (String::from_utf8_lossy(&self.head).into_owned(), None)
        };
        Ran {
            output: ToolOutput {
                content: vec![OutputBlock::Text { text }],
                is_error,
                untrusted: false,
            },
            spill,
        }
    }
}
