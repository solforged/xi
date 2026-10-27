//! Bounded PNG previews. Graphics are emitted only by the screen, never as text.

use std::io::Cursor;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::{ImageFormat, ImageReader, Limits};

use crate::style::Line;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Protocol {
    Kitty,
    Iterm,
}

impl Protocol {
    pub(crate) fn detect() -> Option<Self> {
        let env = |key| std::env::var(key).unwrap_or_default();
        Self::select(
            &env("XI_IMAGES"),
            &env("TERM"),
            &env("TERM_PROGRAM"),
            !env("TMUX").is_empty() || !env("STY").is_empty(),
        )
    }

    fn select(setting: &str, term: &str, program: &str, multiplexed: bool) -> Option<Self> {
        match setting {
            "off" => None,
            "kitty" => Some(Self::Kitty),
            "iterm2" => Some(Self::Iterm),
            "" | "auto" if !multiplexed && term != "dumb" => match program {
                "ghostty" | "kitty" => Some(Self::Kitty),
                "iTerm.app" | "WezTerm" => Some(Self::Iterm),
                _ if term == "xterm-kitty" || term == "xterm-ghostty" => Some(Self::Kitty),
                _ => None,
            },
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Preview {
    png: String,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
}

/// A reserved row. Keeping each row tagged lets the screen reject clipped images.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageRow {
    pub(crate) preview: Arc<Preview>,
    pub(crate) row: usize,
    pub(crate) col: usize,
}

pub(crate) fn rows(media_type: &str, data: &str, cols: usize) -> Vec<Line> {
    if Protocol::detect().is_none() {
        return Vec::new();
    }
    prepare(media_type, data, cols).unwrap_or_else(|| {
        vec![
            Line::default()
                .dim("[image: preview unavailable]")
                .truncate(cols),
        ]
    })
}

fn prepare(media_type: &str, data: &str, cols: usize) -> Option<Vec<Line>> {
    // Bound both the wire payload and decoded pixels before allocating them.
    if media_type != "image/png" || data.len() > 8 * 1024 * 1024 || cols == 0 {
        return None;
    }
    let bytes = STANDARD.decode(data).ok()?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(16_384);
    limits.max_image_height = Some(16_384);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return None;
    }
    // Terminal cells are approximately twice as tall as they are wide.
    // Explicit cell dimensions keep previews within the transcript column.
    let thumb = image.thumbnail(width.min((cols.min(48) * 8) as u32), height.min(12 * 16));
    let preview = Arc::new(Preview {
        cols: (thumb.width() as usize).div_ceil(8),
        rows: (thumb.height() as usize).div_ceil(16),
        png: {
            let mut png = Cursor::new(Vec::new());
            thumb.write_to(&mut png, ImageFormat::Png).ok()?;
            STANDARD.encode(png.into_inner())
        },
    });
    let mut lines = vec![
        Line::default()
            .dim(format!("[image {width}×{height}]"))
            .truncate(cols),
    ];
    for row in 0..preview.rows {
        let mut line = Line::default().ink(" ".repeat(preview.cols));
        line.1 = Some(ImageRow {
            preview: preview.clone(),
            row,
            col: 0,
        });
        lines.push(line);
    }
    Some(lines)
}

/// Returns the complete preview ending on this row, including its column offset.
pub(crate) fn ending_at(lines: &[Line], end: usize, cols: usize) -> Option<&ImageRow> {
    let last = lines.get(end)?.1.as_ref()?;
    let image = &last.preview;
    if last.row + 1 != image.rows || last.col + image.cols > cols {
        return None;
    }
    let start = (end + 1).checked_sub(image.rows)?;
    lines[start..=end]
        .iter()
        .enumerate()
        .all(|(row, line)| {
            line.1.as_ref().is_some_and(|part| {
                part.row == row && part.col == last.col && Arc::ptr_eq(&part.preview, image)
            })
        })
        .then_some(last)
}

impl ImageRow {
    /// Draws from the last reserved row, then restores the text cursor.
    pub(crate) fn write(&self, out: &mut String, protocol: Protocol, id: u32) {
        use std::fmt::Write as _;
        let image = &self.preview;
        out.push_str("\x1b7\r");
        if image.rows > 1 {
            let _ = write!(out, "\x1b[{}A", image.rows - 1);
        }
        if self.col > 0 {
            let _ = write!(out, "\x1b[{}C", self.col);
        }
        match protocol {
            Protocol::Kitty => {
                let chunks = image.png.as_bytes().chunks(4096);
                let count = chunks.len();
                for (index, chunk) in chunks.enumerate() {
                    let more = usize::from(index + 1 < count);
                    if index == 0 {
                        let _ = write!(
                            out,
                            "\x1b_Ga=T,f=100,t=d,q=2,C=1,i={id},c={},r={},m={more};",
                            image.cols, image.rows
                        );
                    } else {
                        let _ = write!(out, "\x1b_Gq=2,m={more};");
                    }
                    // Base64 is ASCII and chunks never split a code point.
                    out.push_str(std::str::from_utf8(chunk).expect("base64"));
                    out.push_str("\x1b\\");
                }
            }
            Protocol::Iterm => {
                let _ = write!(
                    out,
                    "\x1b]1337;File=inline=1;width={};height={};preserveAspectRatio=1:{}\x07",
                    image.cols, image.rows, image.png
                );
            }
        }
        out.push_str("\x1b8");
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn lines() -> Vec<Line> {
        prepare("image/png", &png(), 80).unwrap()
    }

    pub(crate) fn png() -> String {
        let image = image::DynamicImage::new_rgb8(80, 64);
        let mut bytes = Cursor::new(Vec::new());
        image.write_to(&mut bytes, ImageFormat::Png).unwrap();
        STANDARD.encode(bytes.into_inner())
    }

    #[test]
    fn detection_is_conservative_and_can_be_overridden() {
        assert_eq!(
            Protocol::select("", "xterm-kitty", "", false),
            Some(Protocol::Kitty)
        );
        assert_eq!(
            Protocol::select("auto", "xterm-256color", "ghostty", false),
            Some(Protocol::Kitty)
        );
        assert_eq!(
            Protocol::select("", "xterm-256color", "iTerm.app", false),
            Some(Protocol::Iterm)
        );
        assert_eq!(Protocol::select("off", "xterm-kitty", "kitty", false), None);
        assert_eq!(Protocol::select("", "xterm-kitty", "kitty", true), None);
        assert_eq!(Protocol::select("", "dumb", "ghostty", false), None);
        assert_eq!(Protocol::select("", "xterm-256color", "", false), None);
        assert_eq!(
            Protocol::select("kitty", "screen", "", true),
            Some(Protocol::Kitty)
        );
    }

    #[test]
    fn previews_are_bounded_and_only_complete_blocks_draw() {
        let lines = prepare("image/png", &png(), 6).unwrap();
        assert!(lines.iter().all(|line| line.width() <= 6));
        assert!(lines.len() <= 13);
        let end = lines.len() - 1;
        assert!(ending_at(&lines, end, 6).is_some());
        assert!(ending_at(&lines[2..], end - 2, 6).is_none());
        assert!(ending_at(&lines, end, 2).is_none());
        let shifted: Vec<_> = lines
            .into_iter()
            .map(|line| Line::from("  ").extend(line))
            .collect();
        assert_eq!(ending_at(&shifted, end, 8).unwrap().col, 2);
        let clipped: Vec<_> = shifted.into_iter().map(|line| line.truncate(4)).collect();
        assert!(ending_at(&clipped, end, 8).is_none());
    }

    #[test]
    fn invalid_or_unsupported_data_has_no_graphics() {
        for (media, data) in [
            ("image/png", "bad"),
            ("image/png", "YWJj"),
            ("image/jpeg", "YWJj"),
        ] {
            assert!(prepare(media, data, 80).is_none());
        }
        assert!(prepare("image/png", &png(), 0).is_none());
    }

    #[test]
    fn protocols_restore_the_cursor_and_do_not_embed_raw_input() {
        let lines = prepare("image/png", &png(), 80).unwrap();
        let image = ending_at(&lines, lines.len() - 1, 80).unwrap();
        for protocol in [Protocol::Kitty, Protocol::Iterm] {
            let mut out = String::new();
            image.write(&mut out, protocol, 7);
            assert!(out.starts_with("\x1b7\r\x1b[3A"));
            assert!(out.ends_with("\x1b8"));
            if protocol == Protocol::Kitty {
                assert!(out.contains("i=7,c=10,r=4,m=0;"));
            } else {
                assert!(out.contains("width=10;height=4;preserveAspectRatio=1:"));
            }
        }
    }
}
