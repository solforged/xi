//! Terminal clipboard transfer through OSC 52.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// The OSC 52 sequence that sets the system clipboard: `ESC ] 52 ; c ; <base64> BEL`.
#[must_use]
pub fn osc52(text: &str) -> String {
    let mut sequence = String::with_capacity(8 + text.len().div_ceil(3) * 4);
    sequence.push_str("\x1b]52;c;");
    STANDARD.encode_string(text, &mut sequence);
    sequence.push('\x07');
    sequence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_empty_multiline_and_unicode_clipboards() {
        assert_eq!(osc52(""), "\x1b]52;c;\x07");
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52("a\nb"), "\x1b]52;c;YQpi\x07");
        assert_eq!(osc52("ξ"), "\x1b]52;c;zr4=\x07");
    }
}
