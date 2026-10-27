//! The deck's raised surface: the terminal's own background, lifted a little.
//!
//! xi asks the terminal for its background with OSC 11 and then for its
//! primary device attributes, which every terminal answers. The second
//! answer ends the wait, so a terminal that ignores OSC 11 costs one round
//! trip, not the timeout. Without an answer the deck draws no surfaces.

use crate::style::Rgb;

/// How far the surface moves from the background: toward white on a dark
/// theme, toward black on a light one, in parts of 255.
const LIFT: u16 = 16;

/// Asks the terminal on stdin and stdout for its background and returns
/// the raised surface. Needs raw mode and nothing else reading stdin.
#[cfg(unix)]
#[must_use]
pub fn query() -> Option<Rgb> {
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::time::{Duration, Instant};

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    /// How long the terminal gets to answer both requests.
    const WAIT: Duration = Duration::from_millis(250);

    let mut out = std::io::stdout();
    out.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
    out.flush().ok()?;
    let stdin = std::io::stdin();
    let deadline = Instant::now() + WAIT;
    let mut reply = Vec::new();
    let mut buf = [0u8; 256];
    while !answered(&reply) {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(timeout) = PollTimeout::try_from(left.max(Duration::from_millis(1))) else {
            break;
        };
        let mut fds = [PollFd::new(stdin.as_fd(), PollFlags::POLLIN)];
        if left.is_zero() || !matches!(poll(&mut fds, timeout), Ok(n) if n > 0) {
            break;
        }
        match nix::unistd::read(stdin.as_fd(), &mut buf) {
            Ok(n) if n > 0 => reply.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    background(&reply).map(raise)
}

/// Terminals elsewhere draw the deck without surfaces.
#[cfg(not(unix))]
#[must_use]
pub fn query() -> Option<Rgb> {
    None
}

/// Whether `reply` holds the device attributes answer, `CSI ? … c`, which
/// the terminal sends after any OSC 11 answer.
fn answered(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(at, w)| {
        w == b"\x1b[?"
            && reply[at + 3..]
                .iter()
                .find(|b| !(b.is_ascii_digit() || **b == b';'))
                == Some(&b'c')
    })
}

/// The colour in an OSC 11 answer, `ESC ] 11 ; rgb:R/G/B` ended by BEL or
/// ST, with one to four hex digits per channel.
pub(crate) fn background(reply: &[u8]) -> Option<Rgb> {
    let text = std::str::from_utf8(reply).ok()?;
    let start = text.find("\x1b]11;rgb:")? + "\x1b]11;rgb:".len();
    let rest = &text[start..];
    let end = rest.find(['\x07', '\x1b'])?;
    let mut channels = rest[..end].split('/').map(|hex| {
        if hex.is_empty() || hex.len() > 4 {
            return None;
        }
        let value = u32::from_str_radix(hex, 16).ok()?;
        let max = (1u32 << (4 * hex.len())) - 1;
        u8::try_from(value * 255 / max).ok()
    });
    let rgb = [channels.next()??, channels.next()??, channels.next()??];
    channels.next().is_none().then_some(rgb)
}

/// `bg` moved [`LIFT`] toward white when it is dark, toward black when
/// it is light.
pub(crate) fn raise(bg: Rgb) -> Rgb {
    let [r, g, b] = bg.map(u32::from);
    // Rec. 709 luma, in parts of 255.
    let dark = (r * 2126 + g * 7152 + b * 722) / 10_000 < 128;
    bg.map(|c| {
        let c = u16::from(c);
        let moved = if dark {
            c + (255 - c) * LIFT / 255
        } else {
            c - c * LIFT / 255
        };
        u8::try_from(moved).unwrap_or(u8::MAX)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backgrounds_parse_from_either_terminator_and_any_digit_count() {
        let reply = b"\x1b]11;rgb:1616/1010/0e0e\x1b\\\x1b[?62;22c";
        assert_eq!(background(reply), Some([0x16, 0x10, 0x0e]));
        assert!(answered(reply));
        assert_eq!(background(b"\x1b]11;rgb:ff/80/0\x07"), Some([255, 128, 0]));
        assert_eq!(background(b"\x1b]11;rgb:f/8/0\x07"), Some([255, 136, 0]));
        assert_eq!(background(b"\x1b]11;rgb:1/2\x07"), None);
        assert_eq!(background(b"\x1b]11;rgb:1/2/3"), None, "unterminated");
        // A terminal without OSC 11 answers only the attributes request.
        let bare = b"\x1b[?1;2c";
        assert!(answered(bare));
        assert_eq!(background(bare), None);
        assert!(!answered(b"\x1b]11;rgb:1/2/3\x07\x1b[?1;2"));
    }

    #[test]
    fn surfaces_lift_dark_themes_and_sink_light_ones() {
        let dark = raise([22, 16, 14]);
        assert!(
            dark.iter()
                .zip([22, 16, 14])
                .all(|(s, b)| *s > b && *s < b + 20)
        );
        let light = raise([250, 248, 240]);
        assert!(
            light
                .iter()
                .zip([250, 248, 240])
                .all(|(s, b)| *s < b && *s + 20 > b)
        );
        assert_eq!(raise([255, 255, 255]), [239, 239, 239]);
        assert_eq!(raise([0, 0, 0]), [16, 16, 16]);
    }
}
