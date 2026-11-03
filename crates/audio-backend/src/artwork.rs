//! Thumbnail-only bounded decoding, away from native real-time callbacks.
use image::{ImageReader, Limits};
use std::{io::Cursor, sync::Arc};
pub const MAX_NATIVE_BYTES: usize = 256 * 1024;
pub const MAX_BYTES: usize = 48 * 1024;
pub const MAX_EDGE: u32 = 192;
pub type Artwork = Arc<Thumbnail>;
#[derive(PartialEq, Eq)]
pub struct Thumbnail {
    pub id: [u8; 32],
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    input_id: [u8; 32],
}
impl std::fmt::Debug for Thumbnail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Artwork([REDACTED])")
    }
}
pub fn normalize(encoded: &[u8], cached: Option<&Artwork>) -> Option<Artwork> {
    if encoded.is_empty() || encoded.len() > MAX_NATIVE_BYTES {
        return None;
    }
    let input_id: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, encoded)
        .as_ref()
        .try_into()
        .ok()?;
    if let Some(cached) = cached.filter(|cached| cached.input_id == input_id) {
        return Some(cached.clone());
    }
    let reader = ImageReader::new(Cursor::new(encoded))
        .with_guessed_format()
        .ok()?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg)
    ) {
        return None;
    }
    let (width, height) = reader.into_dimensions().ok()?;
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > 4_194_304 {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(encoded))
        .with_guessed_format()
        .ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let thumbnail = reader
        .decode()
        .ok()?
        .thumbnail(MAX_EDGE, MAX_EDGE)
        .to_rgb8();
    let mut data = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut data, 75)
        .encode_image(&thumbnail)
        .ok()?;
    if data.is_empty() || data.len() > MAX_BYTES {
        return None;
    }
    let id = ring::digest::digest(&ring::digest::SHA256, &data)
        .as_ref()
        .try_into()
        .ok()?;
    Some(Arc::new(Thumbnail {
        id,
        data,
        width: thumbnail.width(),
        height: thumbnail.height(),
        input_id,
    }))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn real_png_normalizes_once_to_bounded_jpeg_and_content_identity() {
        let pixels = image::RgbImage::from_pixel(400, 200, image::Rgb([36, 72, 144]));
        let mut input = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(pixels)
            .write_to(&mut input, image::ImageFormat::Png)
            .unwrap();
        let a = normalize(input.get_ref(), None).unwrap();
        let b = normalize(input.get_ref(), Some(&a)).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a, b);
        assert_eq!((a.width, a.height), (192, 96));
        assert!(a.data.len() <= MAX_BYTES);
        assert_eq!(
            image::guess_format(&a.data).unwrap(),
            image::ImageFormat::Jpeg
        );
        assert!(normalize(&vec![0; MAX_NATIVE_BYTES + 1], None).is_none());
        assert!(normalize(b"private non-image bytes", None).is_none());
    }
}
