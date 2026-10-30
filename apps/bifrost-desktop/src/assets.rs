//! Shared application icon, already decoded to straight-alpha RGBA.

const ICON_RGBA: &[u8; 32 * 32 * 4] = include_bytes!("../../bifrost/assets/icon-32.rgba");

/// Return the icon width, height and top-to-bottom RGBA pixels without allocation.
pub fn icon_rgba() -> (u32, u32, &'static [u8]) {
    (32, 32, ICON_RGBA)
}
