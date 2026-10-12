//! A QR code as a GTK texture, for the address a phone should open.
//!
//! Drawn straight from the module matrix: dark modules black, the rest white,
//! a quiet zone around it. No image library involved.

use relm4::gtk::{gdk, glib};

/// Pixels per module. The picture widget scales it further; this only has to
/// be large enough that scaling never blurs a module into its neighbour.
const SCALE: usize = 8;
/// Modules of white around the code, as the standard asks for.
const QUIET_ZONE: usize = 2;

/// The code for `text`, or `None` when the text does not fit a QR code.
pub fn texture(text: &str) -> Option<gdk::Texture> {
    let (side, rgba) = rgba(text)?;
    let texture = gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(rgba),
        side * 4,
    );
    Some(texture.into())
}

/// The code as RGBA rows `side` pixels wide, or `None` when `text` does not
/// fit. Kept apart from [`texture`] because a texture needs a display.
fn rgba(text: &str) -> Option<(usize, Vec<u8>)> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let modules = code.width();
    let colors = code.to_colors();
    let side = (modules + 2 * QUIET_ZONE) * SCALE;
    let mut rgba = vec![0xffu8; side * side * 4];
    for (index, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let (row, col) = (index / modules + QUIET_ZONE, index % modules + QUIET_ZONE);
        for y in row * SCALE..(row + 1) * SCALE {
            for x in col * SCALE..(col + 1) * SCALE {
                let at = (y * side + x) * 4;
                rgba[at..at + 3].copy_from_slice(&[0, 0, 0]);
            }
        }
    }
    Some((side, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_sit_inside_quiet_zone() {
        let (side, pixels) = rgba("http://192.168.1.20:8080").expect("short address fits");
        let pixel = |x: usize, y: usize| &pixels[(y * side + x) * 4..(y * side + x) * 4 + 4];
        let margin = QUIET_ZONE * SCALE;
        assert_eq!(pixels.len(), side * side * 4);
        assert_eq!(pixel(margin - 1, margin - 1), [0xff; 4], "quiet zone");
        // Every code starts with a dark finder pattern in its top-left corner.
        assert_eq!(pixel(margin, margin), [0, 0, 0, 0xff]);
        assert_eq!(
            pixel(margin + SCALE - 1, margin + SCALE - 1),
            [0, 0, 0, 0xff]
        );
        assert!(rgba("a".repeat(5000).as_str()).is_none());
    }
}
