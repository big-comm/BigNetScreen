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
    let texture = gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &glib::Bytes::from_owned(rgba),
        side * 4,
    );
    Some(texture.into())
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_short_address_fits_and_a_novel_does_not() {
        // Textures need a display; only the matrix logic is exercised here.
        assert!(qrcode::QrCode::new(b"http://192.168.1.20:8080").is_ok());
        assert!(qrcode::QrCode::new(vec![b'a'; 5000]).is_err());
    }
}
