//! Clipboard image input and terminal text transfer through OSC 52.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use xi_protocol::Block;

use crate::Error;

/// Maximum encoded image size, before base64, accepted for an attachment.
const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Reads the local system clipboard as an inline PNG image.
///
/// Clipboard access and encoding are blocking; call this off the UI task.
///
/// # Errors
///
/// Returns an error when the clipboard is unavailable, has no image, or the
/// image is invalid or too large.
pub fn image() -> Result<Block, Error> {
    let mut clipboard = arboard::Clipboard::new()?;
    encode_image(clipboard.get_image()?)
}

fn encode_image(image: arboard::ImageData<'_>) -> Result<Block, Error> {
    let width = u32::try_from(image.width)?;
    let height = u32::try_from(image.height)?;
    let bytes = image
        .width
        .checked_mul(image.height)
        .and_then(|n| n.checked_mul(4));
    if width == 0 || height == 0 || bytes != Some(image.bytes.len()) {
        return Err("invalid clipboard image dimensions".into());
    }
    if image.bytes.len() > 100 * 1024 * 1024 {
        return Err("clipboard image is too large (maximum 100 MiB of pixels)".into());
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png).write_image(&image.bytes, width, height, ExtendedColorType::Rgba8)?;
    if png.len() > MAX_IMAGE_BYTES {
        return Err("clipboard image is too large (maximum 5 MiB PNG)".into());
    }
    Ok(Block::Image {
        media_type: "image/png".into(),
        data: STANDARD.encode(png),
    })
}

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
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn clipboard_pixels_become_an_inline_png() {
        let pixels = vec![255, 0, 0, 255, 0, 255, 0, 128];
        let block = encode_image(arboard::ImageData {
            width: 2,
            height: 1,
            bytes: Cow::Borrowed(&pixels),
        })
        .unwrap();
        let Block::Image { media_type, data } = block else {
            panic!("expected image");
        };
        assert_eq!(media_type, "image/png");
        let png = STANDARD.decode(data).unwrap();
        let decoded = image::load_from_memory(&png).unwrap().into_rgba8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.as_raw(), &pixels);
    }

    #[test]
    fn invalid_clipboard_images_are_rejected() {
        for (width, height, bytes) in [(0, 1, vec![]), (1, 1, vec![0; 3]), (usize::MAX, 2, vec![])]
        {
            assert!(
                encode_image(arboard::ImageData {
                    width,
                    height,
                    bytes: Cow::Owned(bytes),
                })
                .is_err()
            );
        }
    }

    #[test]
    fn encodes_empty_multiline_and_unicode_clipboards() {
        assert_eq!(osc52(""), "\x1b]52;c;\x07");
        assert_eq!(osc52("hello"), "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(osc52("a\nb"), "\x1b]52;c;YQpi\x07");
        assert_eq!(osc52("ξ"), "\x1b]52;c;zr4=\x07");
    }
}
