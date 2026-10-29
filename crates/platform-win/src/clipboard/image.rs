//! Pure, bounded PNG / packed Windows DIB conversion; never accesses the clipboard.

use std::io::{self, Cursor, Write};

use clipboard::ClipboardError;

/// Hard pixel-axis limit, checked from headers before allocating decoded output.
pub const MAX_IMAGE_DIMENSION: u32 = 16_384;
/// Hard RGBA working-frame limit (128 MiB), independent of compressed PNG size.
pub const MAX_DECODED_RGBA_BYTES: usize = 128 * 1024 * 1024;
const MAX_PNG_BYTES: usize = clipboard::MAX_IMAGE_BYTES as usize;
const V5_HEADER: usize = 124;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Checks dimensions and decoded size without allocating or decoding pixels.
pub fn checked_rgba_bytes(width: u32, height: u32) -> Result<usize, ClipboardError> {
    if width == 0 || height == 0 {
        return Err(ClipboardError::Invalid);
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(ClipboardError::TooLarge);
    }
    let bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ClipboardError::TooLarge)?;
    if bytes > MAX_DECODED_RGBA_BYTES {
        return Err(ClipboardError::TooLarge);
    }
    Ok(bytes)
}

fn u16_at(data: &[u8], offset: usize) -> Result<u16, ClipboardError> {
    let bytes = data
        .get(offset..offset + 2)
        .ok_or(ClipboardError::Invalid)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32, ClipboardError> {
    let bytes = data
        .get(offset..offset + 4)
        .ok_or(ClipboardError::Invalid)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

struct Dib {
    width: u32,
    height: u32,
    top_down: bool,
    bpp: usize,
    stride: usize,
    offset: usize,
    end: usize,
    rgba_bytes: usize,
    masks: [u32; 4],
    rgb_zero_alpha: bool,
}

fn valid_masks(masks: [u32; 4], bits: u16) -> bool {
    let mut used = 0;
    for (index, mask) in masks.into_iter().enumerate() {
        if mask == 0 {
            if index < 3 {
                return false;
            }
            continue;
        }
        if bits < 32 && mask >> bits != 0 {
            return false;
        }
        let shifted = mask >> mask.trailing_zeros();
        if shifted & shifted.wrapping_add(1) != 0 || used & mask != 0 {
            return false;
        }
        used |= mask;
    }
    true
}

fn parse_dib(data: &[u8]) -> Result<Option<Dib>, ClipboardError> {
    let header = u32_at(data, 0)? as usize;
    // OS/2 headers and other layouts are not Windows BITMAPINFO variants we support.
    if !matches!(header, 40 | 52 | 56 | 108 | 124) {
        return Ok(None);
    }
    if data.len() < header {
        return Err(ClipboardError::Invalid);
    }
    let bits = u16_at(data, 14)?;
    let compression = u32_at(data, 16)?;
    if !matches!(bits, 24 | 32) || !matches!(compression, 0 | 3) {
        return Ok(None);
    }
    let width = u32_at(data, 4)? as i32;
    let signed_height = u32_at(data, 8)? as i32;
    if width <= 0 || signed_height == 0 || signed_height == i32::MIN || u16_at(data, 12)? != 1 {
        return Err(ClipboardError::Invalid);
    }
    let width = width as u32;
    let height = signed_height.unsigned_abs();
    let rgba_bytes = checked_rgba_bytes(width, height)?;
    let mut offset = header;
    let masks = if compression == 3 {
        if header == 40 {
            offset += 12;
        }
        [
            u32_at(data, 40)?,
            u32_at(data, 44)?,
            u32_at(data, 48)?,
            if header >= 56 { u32_at(data, 52)? } else { 0 },
        ]
    } else {
        [
            0x00ff_0000,
            0x0000_ff00,
            0x0000_00ff,
            if bits == 32 { 0xff00_0000 } else { 0 },
        ]
    };
    if !valid_masks(masks, bits) {
        return Err(ClipboardError::Invalid);
    }
    let colors = u32_at(data, 32)? as usize;
    // True-color optimization palettes are small; do not snapshot arbitrary metadata.
    if colors > 256 {
        return Ok(None);
    }
    offset = offset
        .checked_add(colors * 4)
        .ok_or(ClipboardError::TooLarge)?;
    let bpp = usize::from(bits / 8);
    let stride = (width as usize)
        .checked_mul(bpp)
        .and_then(|bytes| bytes.checked_add(3))
        .map(|bytes| bytes & !3)
        .ok_or(ClipboardError::TooLarge)?;
    let pixel_bytes = stride
        .checked_mul(height as usize)
        .ok_or(ClipboardError::TooLarge)?;
    let end = offset
        .checked_add(pixel_bytes)
        .ok_or(ClipboardError::TooLarge)?;
    if end > data.len() {
        return Err(ClipboardError::Invalid);
    }
    let advertised = u32_at(data, 20)? as usize;
    if advertised != 0 && advertised != pixel_bytes {
        return Err(ClipboardError::Invalid);
    }
    if header == V5_HEADER {
        let profile_start = u32_at(data, 112)? as usize;
        let profile_size = u32_at(data, 116)? as usize;
        // Profiles after the pixels can be omitted; overlapping/preceding profiles are
        // unsupported because their packed-DIB pixel offset is producer-dependent.
        if profile_size != 0 && profile_start < end {
            return Ok(None);
        }
    }
    Ok(Some(Dib {
        width,
        height,
        top_down: signed_height < 0,
        bpp,
        stride,
        offset,
        end,
        rgba_bytes,
        masks,
        rgb_zero_alpha: compression == 0 && bits == 32,
    }))
}

/// Validates a supported DIB and returns only the bounded prefix needed by the codec.
/// Callers can copy this prefix under the clipboard lock and encode after unlocking.
pub fn dib_storage_bytes(dib: &[u8]) -> Result<Option<usize>, ClipboardError> {
    Ok(parse_dib(dib)?.map(|info| info.end))
}

fn channel(pixel: u32, mask: u32, absent: u8) -> u8 {
    if mask == 0 {
        return absent;
    }
    let shift = mask.trailing_zeros();
    let maximum = u64::from(mask >> shift);
    let value = u64::from((pixel & mask) >> shift);
    ((value * 255 + maximum / 2) / maximum) as u8
}

/// Converts supported 24/32-bit RGB or bitfield DIBs to a bounded PNG.
/// Unsupported compression/depth/header layouts return `None`; broken supported DIBs fail.
pub fn dib_to_png(dib: &[u8]) -> Result<Option<Vec<u8>>, ClipboardError> {
    let Some(info) = parse_dib(dib)? else {
        return Ok(None);
    };
    let mut rgba = vec![0; info.rgba_bytes];
    let mut any_alpha = false;
    for y in 0..info.height as usize {
        let source_y = if info.top_down {
            y
        } else {
            info.height as usize - 1 - y
        };
        let row = info.offset + source_y * info.stride;
        for x in 0..info.width as usize {
            let start = row + x * info.bpp;
            let bytes = &dib[start..start + info.bpp];
            let pixel = u32::from_le_bytes([
                bytes[0],
                bytes[1],
                bytes[2],
                if info.bpp == 4 { bytes[3] } else { 0 },
            ]);
            let target = (y * info.width as usize + x) * 4;
            for (component, mask) in info.masks.into_iter().enumerate() {
                rgba[target + component] = channel(pixel, mask, 255);
            }
            any_alpha |= rgba[target + 3] != 0;
        }
    }
    // Legacy BI_RGB producers leave the reserved byte zero instead of supplying alpha.
    if info.rgb_zero_alpha && !any_alpha {
        for pixel in rgba.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
    }
    encode_rgba(info.width, info.height, &rgba).map(Some)
}

#[derive(Default)]
struct BoundedPng {
    bytes: Vec<u8>,
    exceeded: bool,
}

impl Write for BoundedPng {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_PNG_BYTES - self.bytes.len() {
            self.exceeded = true;
            return Err(io::Error::other("PNG size limit"));
        }
        let needed = self.bytes.len() + bytes.len();
        if needed > self.bytes.capacity() {
            // Bound capacity as well as length, without quadratic exact-size growth.
            let capacity = needed
                .max(self.bytes.capacity().saturating_mul(2))
                .min(MAX_PNG_BYTES);
            self.bytes.reserve_exact(capacity - self.bytes.len());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    let mut output = BoundedPng::default();
    let result = (|| {
        let mut encoder = png::Encoder::new(&mut output, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
        writer.finish()
    })();
    if output.exceeded {
        return Err(ClipboardError::TooLarge);
    }
    result.map_err(|_| ClipboardError::Invalid)?;
    Ok(output.bytes)
}

fn decode_error(error: png::DecodingError) -> ClipboardError {
    match error {
        png::DecodingError::LimitsExceeded => ClipboardError::TooLarge,
        _ => ClipboardError::Invalid,
    }
}

fn reject_extra_animation_frames(png: &[u8]) -> Result<(), ClipboardError> {
    let mut offset = PNG_SIGNATURE.len();
    let mut frame_control = false;
    let mut image_data = false;
    while offset < png.len() {
        let header = png.get(offset..offset + 8).ok_or(ClipboardError::Invalid)?;
        let length = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
        let end = offset
            .checked_add(12)
            .and_then(|start| start.checked_add(length))
            .ok_or(ClipboardError::Invalid)?;
        if end > png.len() {
            return Err(ClipboardError::Invalid);
        }
        match &header[4..8] {
            b"fdAT" => return Err(ClipboardError::Invalid),
            b"fcTL" => {
                if frame_control || image_data {
                    return Err(ClipboardError::Invalid);
                }
                frame_control = true;
            }
            b"IDAT" => image_data = true,
            b"IEND" => return Ok(()),
            _ => {}
        }
        offset = end;
    }
    Err(ClipboardError::Invalid)
}

/// Decodes the PNG default (IDAT) image, never APNG animation frames.
/// APNG is accepted only when its sole animation frame is the default image.
/// The returned eager DIBV5 is top-down, straight-alpha BGRA with explicit masks.
pub fn png_to_dibv5(png: &[u8]) -> Result<Vec<u8>, ClipboardError> {
    if png.len() > MAX_PNG_BYTES {
        return Err(ClipboardError::TooLarge);
    }
    if !png.starts_with(PNG_SIGNATURE) {
        return Err(ClipboardError::Invalid);
    }
    let mut decoder = png::Decoder::new(Cursor::new(png));
    decoder.set_limits(png::Limits {
        bytes: MAX_DECODED_RGBA_BYTES,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    // Inspect IHDR before read_info can allocate decoder frame/metadata buffers.
    let header = decoder.read_header_info().map_err(decode_error)?;
    let (width, height) = (header.width, header.height);
    let rgba_bytes = checked_rgba_bytes(width, height)?;
    // Enforce our default-frame-only policy independently of the advertised frame count.
    // CRCs and all PNG semantics remain the decoder's responsibility.
    reject_extra_animation_frames(png)?;
    let mut reader = decoder.read_info().map_err(decode_error)?;
    if let Some(animation) = reader.info().animation_control.as_ref() {
        if animation.num_frames != 1 || reader.info().frame_control.is_none() {
            return Err(ClipboardError::Invalid);
        }
    }
    let decoded_bytes = reader
        .output_buffer_size()
        .ok_or(ClipboardError::TooLarge)?;
    if decoded_bytes > rgba_bytes {
        return Err(ClipboardError::TooLarge);
    }
    let mut dib = vec![0; V5_HEADER + rgba_bytes];
    let frame = reader
        .next_frame(&mut dib[V5_HEADER..])
        .map_err(decode_error)?;
    if frame.width != width || frame.height != height || frame.bit_depth != png::BitDepth::Eight {
        return Err(ClipboardError::Invalid);
    }
    reader.finish().map_err(decode_error)?;
    let samples = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err(ClipboardError::Invalid),
    };
    // Expand backwards inside the eventual DIB pixel allocation, avoiding a second frame.
    let pixels = &mut dib[V5_HEADER..];
    for index in (0..rgba_bytes / 4).rev() {
        let source = index * samples;
        let (red, green, blue, alpha) = match samples {
            1 => (pixels[source], pixels[source], pixels[source], 255),
            2 => (
                pixels[source],
                pixels[source],
                pixels[source],
                pixels[source + 1],
            ),
            3 => (pixels[source], pixels[source + 1], pixels[source + 2], 255),
            _ => (
                pixels[source],
                pixels[source + 1],
                pixels[source + 2],
                pixels[source + 3],
            ),
        };
        pixels[index * 4..index * 4 + 4].copy_from_slice(&[blue, green, red, alpha]);
    }
    for (offset, value) in [
        (0, V5_HEADER as u32),
        (4, width),
        (8, (-(height as i32)) as u32),
        (16, 3),
        (20, rgba_bytes as u32),
        (40, 0x00ff_0000),
        (44, 0x0000_ff00),
        (48, 0x0000_00ff),
        (52, 0xff00_0000),
        (56, 0x7352_4742), // LCS_sRGB
        (108, 4),          // LCS_GM_IMAGES
    ] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put32(data: &mut [u8], offset: usize, value: u32) {
        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn dib(width: u32, height: i32, bits: u16, compression: u32) -> Vec<u8> {
        let header = if compression == 3 { 56 } else { 40 };
        let stride = (width as usize * usize::from(bits / 8) + 3) & !3;
        let mut bytes = vec![0; header + stride * height.unsigned_abs() as usize];
        put32(&mut bytes, 0, header as u32);
        put32(&mut bytes, 4, width);
        put32(&mut bytes, 8, height as u32);
        bytes[12..14].copy_from_slice(&1u16.to_le_bytes());
        bytes[14..16].copy_from_slice(&bits.to_le_bytes());
        put32(&mut bytes, 16, compression);
        if compression == 3 {
            for (offset, mask) in [(40, 0xff0000), (44, 0xff00), (48, 0xff)] {
                put32(&mut bytes, offset, mask);
            }
            if bits == 32 {
                put32(&mut bytes, 52, 0xff000000);
            }
        }
        bytes
    }

    fn rgba(png: &[u8]) -> Vec<u8> {
        let dib = png_to_dibv5(png).unwrap();
        dib[V5_HEADER..]
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|pixel| [pixel[2], pixel[1], pixel[0], pixel[3]])
            .collect()
    }

    fn converted(dib: &[u8]) -> Vec<u8> {
        rgba(&dib_to_png(dib).unwrap().unwrap())
    }

    fn fixture(
        color: png::ColorType,
        depth: png::BitDepth,
        data: &[u8],
        trns: Option<Vec<u8>>,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
            encoder.set_color(color);
            encoder.set_depth(depth);
            if color == png::ColorType::Indexed {
                encoder.set_palette(vec![255, 0, 0, 0, 255, 0]);
            }
            if let Some(trns) = trns {
                encoder.set_trns(trns);
            }
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(data).unwrap();
            writer.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn rgb_and_bitfields_handle_both_row_directions_and_padding() {
        for bits in [24, 32] {
            for compression in [0, 3] {
                for height in [-2, 2] {
                    let mut bytes = dib(1, height, bits, compression);
                    let offset = u32_at(&bytes, 0).unwrap() as usize;
                    let rows = if height < 0 {
                        [[0, 0, 255, 255], [0, 255, 0, 255]]
                    } else {
                        [[0, 255, 0, 255], [0, 0, 255, 255]]
                    };
                    for (y, row) in rows.into_iter().enumerate() {
                        let count = usize::from(bits / 8);
                        bytes[offset + y * 4..offset + y * 4 + count]
                            .copy_from_slice(&row[..count]);
                    }
                    assert_eq!(converted(&bytes), [255, 0, 0, 255, 0, 255, 0, 255]);
                }
            }
        }
    }

    #[test]
    fn rgb_zero_alpha_is_opaque_only_when_every_alpha_is_zero() {
        let mut bytes = dib(2, -1, 32, 0);
        bytes[40..].copy_from_slice(&[3, 2, 1, 0, 6, 5, 4, 0]);
        assert_eq!(converted(&bytes), [1, 2, 3, 255, 4, 5, 6, 255]);
        bytes[47] = 9;
        assert_eq!(converted(&bytes), [1, 2, 3, 0, 4, 5, 6, 9]);
        let mut bitfields = dib(1, -1, 32, 3);
        assert_eq!(converted(&bitfields), [0, 0, 0, 0]);
        put32(&mut bitfields, 52, 0);
        assert_eq!(converted(&bitfields), [0, 0, 0, 255]);
    }

    #[test]
    fn masks_are_disjoint_contiguous_and_within_pixel_width() {
        for (offset, mask) in [(40, 0), (40, 0x00550000), (44, 0xff0000), (52, 0xff)] {
            let mut bytes = dib(1, -1, 32, 3);
            put32(&mut bytes, offset, mask);
            assert_eq!(dib_to_png(&bytes), Err(ClipboardError::Invalid));
        }
        let mut bytes = dib(1, -1, 24, 3);
        put32(&mut bytes, 40, 0xff000000);
        assert_eq!(dib_to_png(&bytes), Err(ClipboardError::Invalid));
        let mut bytes = dib(1, -1, 32, 3);
        for (offset, mask) in [
            (40, 0x3ff00000),
            (44, 0xffc00),
            (48, 0x3ff),
            (52, 0xc0000000),
        ] {
            put32(&mut bytes, offset, mask);
        }
        put32(&mut bytes, 56, 0xfff00000);
        assert_eq!(converted(&bytes), [255, 0, 0, 255]);
    }

    #[test]
    fn bitmapinfo_external_masks_and_v4_headers_are_supported() {
        let mut bytes = dib(1, -1, 32, 3);
        // BITMAPINFOHEADER has three external masks, no explicit alpha channel.
        put32(&mut bytes, 0, 40);
        bytes.drain(52..56);
        bytes[52..56].copy_from_slice(&[3, 2, 1, 0]);
        assert_eq!(converted(&bytes), [1, 2, 3, 255]);
        let mut v4 = dib(1, -1, 32, 3);
        v4.splice(56..56, [0; 52]);
        put32(&mut v4, 0, 108);
        v4[108..112].copy_from_slice(&[3, 2, 1, 7]);
        assert_eq!(converted(&v4), [1, 2, 3, 7]);
    }

    #[test]
    fn png_dibv5_png_roundtrip_preserves_pixels_and_header() {
        let pixels = [1, 2, 3, 0, 254, 127, 63, 200];
        let png = encode_rgba(2, 1, &pixels).unwrap();
        let dib = png_to_dibv5(&png).unwrap();
        assert_eq!(dib.len(), 124 + 8);
        for (offset, value) in [
            (0, 124),
            (4, 2),
            (8, u32::MAX),
            (16, 3),
            (20, 8),
            (40, 0xff0000),
            (44, 0xff00),
            (48, 0xff),
            (52, 0xff000000),
        ] {
            assert_eq!(u32_at(&dib, offset).unwrap(), value);
        }
        assert_eq!(u16_at(&dib, 14).unwrap(), 32);
        assert_eq!(converted(&dib), pixels);
    }

    #[test]
    fn palette_grayscale_sixteen_bit_and_transparency_are_normalized() {
        use png::{BitDepth as D, ColorType as C};
        let cases = [
            (
                fixture(C::Indexed, D::One, &[0b01000000], Some(vec![0, 128])),
                vec![255, 0, 0, 0, 0, 255, 0, 128],
            ),
            (
                fixture(C::Grayscale, D::One, &[0b01000000], None),
                vec![0, 0, 0, 255, 255, 255, 255, 255],
            ),
            (
                fixture(C::Grayscale, D::Eight, &[0, 99], Some(vec![0, 0])),
                vec![0, 0, 0, 0, 99, 99, 99, 255],
            ),
            (
                fixture(C::GrayscaleAlpha, D::Eight, &[10, 20, 30, 40], None),
                vec![10, 10, 10, 20, 30, 30, 30, 40],
            ),
            (
                fixture(
                    C::Rgb,
                    D::Eight,
                    &[1, 2, 3, 4, 5, 6],
                    Some(vec![0, 1, 0, 2, 0, 3]),
                ),
                vec![1, 2, 3, 0, 4, 5, 6, 255],
            ),
            (
                fixture(C::Grayscale, D::Sixteen, &[0x12, 0x34, 0xab, 0xcd], None),
                vec![0x12, 0x12, 0x12, 255, 0xab, 0xab, 0xab, 255],
            ),
            (
                fixture(
                    C::Rgba,
                    D::Sixteen,
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
                    None,
                ),
                vec![1, 3, 5, 7, 9, 11, 13, 15],
            ),
        ];
        for (png, expected) in cases {
            assert_eq!(rgba(&png), expected);
        }
    }

    #[test]
    fn malformed_and_truncated_supported_inputs_fail() {
        let bytes = dib(2, -2, 24, 0);
        for length in 0..bytes.len() {
            assert_eq!(dib_to_png(&bytes[..length]), Err(ClipboardError::Invalid));
        }
        for (offset, value) in [
            (4, 0),
            (4, u32::MAX),
            (8, 0),
            (8, 0x80000000),
            (12, 24 << 16), // Zero planes, while keeping the supported 24bpp field intact.
            (20, 1),
        ] {
            let mut invalid = bytes.clone();
            put32(&mut invalid, offset, value);
            assert_eq!(dib_to_png(&invalid), Err(ClipboardError::Invalid));
        }
        let png = encode_rgba(1, 1, &[1, 2, 3, 4]).unwrap();
        for length in 0..png.len() {
            assert_eq!(png_to_dibv5(&png[..length]), Err(ClipboardError::Invalid));
        }
        let mut corrupt = png;
        corrupt[29] ^= 1; // IHDR CRC.
        assert_eq!(png_to_dibv5(&corrupt), Err(ClipboardError::Invalid));
    }

    #[test]
    fn unsupported_compression_depth_and_metadata_are_not_errors() {
        for compression in [1, 2, 4, 5, 6] {
            let mut bytes = dib(1, 1, 24, 0);
            put32(&mut bytes, 16, compression);
            assert_eq!(dib_to_png(&bytes), Ok(None));
        }
        for bits in [1u16, 4, 8, 16] {
            let mut bytes = dib(1, 1, 24, 0);
            bytes[14..16].copy_from_slice(&bits.to_le_bytes());
            assert_eq!(dib_to_png(&bytes), Ok(None));
        }
        let mut bytes = dib(1, 1, 24, 0);
        put32(&mut bytes, 32, u32::MAX);
        assert_eq!(dib_storage_bytes(&bytes), Ok(None));
    }

    #[test]
    fn limits_are_checked_from_headers_without_large_allocations() {
        assert_eq!(checked_rgba_bytes(16384, 1), Ok(65536));
        assert_eq!(checked_rgba_bytes(1, 16384), Ok(65536));
        assert_eq!(checked_rgba_bytes(16385, 1), Err(ClipboardError::TooLarge));
        assert_eq!(checked_rgba_bytes(1, 16385), Err(ClipboardError::TooLarge));
        assert_eq!(checked_rgba_bytes(8192, 4096), Ok(MAX_DECODED_RGBA_BYTES));
        // RGBA byte counts are multiples of four; increasing this width exceeds the cap.
        assert_eq!(
            checked_rgba_bytes(8193, 4096),
            Err(ClipboardError::TooLarge)
        );
        for (width, height) in [(16385, 1), (1, 16385), (8193, 4096)] {
            let mut header = dib(1, -1, 32, 0);
            header.truncate(40);
            put32(&mut header, 4, width);
            put32(&mut header, 8, height);
            assert_eq!(dib_storage_bytes(&header), Err(ClipboardError::TooLarge));
            let mut png = Vec::new();
            let encoder = png::Encoder::new(&mut png, width, height);
            drop(encoder.write_header().unwrap());
            assert_eq!(png_to_dibv5(&png), Err(ClipboardError::TooLarge));
        }
        let mut boundary = dib(1, -1, 32, 0);
        boundary.truncate(40);
        put32(&mut boundary, 4, 8192);
        put32(&mut boundary, 8, 4096);
        assert_eq!(dib_storage_bytes(&boundary), Err(ClipboardError::Invalid));
    }

    #[test]
    fn snapshot_prefix_omits_unneeded_trailing_storage() {
        let mut bytes = dib(1, -1, 24, 0);
        bytes.extend_from_slice(&[0; 100]);
        assert_eq!(dib_storage_bytes(&bytes), Ok(Some(44)));
        assert_eq!(converted(&bytes[..44]), [0, 0, 0, 255]);
    }

    #[test]
    fn png_byte_cap_is_not_a_dib_byte_cap() {
        let bytes = dib(16384, -513, 32, 0);
        assert!(bytes.len() > MAX_PNG_BYTES);
        let png = dib_to_png(&bytes).unwrap().unwrap();
        assert!(png.len() < MAX_PNG_BYTES);
    }

    #[test]
    fn bounded_writer_rejects_cap_plus_one_without_growing() {
        let mut writer = BoundedPng::default();
        let block = [0; 4096];
        for _ in 0..MAX_PNG_BYTES / block.len() {
            writer.write_all(&block).unwrap();
        }
        assert_eq!(writer.bytes.len(), MAX_PNG_BYTES);
        assert!(writer.bytes.capacity() <= MAX_PNG_BYTES);
        assert!(writer.write_all(&[0]).is_err());
        assert!(writer.exceeded);
        assert_eq!(writer.bytes.len(), MAX_PNG_BYTES);
        assert!(writer.bytes.capacity() <= MAX_PNG_BYTES);
    }

    #[test]
    fn png_source_cap_is_checked_before_signature_or_decode() {
        let bytes = vec![0; MAX_PNG_BYTES + 1];
        assert_eq!(png_to_dibv5(&bytes), Err(ClipboardError::TooLarge));
    }

    fn animated(frames: u32, separate_default: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_animated(frames, 0).unwrap();
            encoder.set_sep_def_img(separate_default).unwrap();
            let mut writer = encoder.write_header().unwrap();
            for index in 0..frames + u32::from(separate_default) {
                writer.write_image_data(&[index as u8, 2, 3, 255]).unwrap();
            }
            writer.finish().unwrap();
        }
        bytes
    }

    #[test]
    fn apng_rejects_frames_beyond_default_and_separate_animation() {
        assert_eq!(
            png_to_dibv5(&animated(2, false)),
            Err(ClipboardError::Invalid)
        );
        assert_eq!(
            png_to_dibv5(&animated(1, true)),
            Err(ClipboardError::Invalid)
        );
        assert_eq!(rgba(&animated(1, false)), [0, 2, 3, 255]);
    }

    #[test]
    fn apng_rejects_unadvertised_animation_data() {
        for kind in [png::chunk::fdAT, png::chunk::fcTL] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                encoder.set_animated(1, 0).unwrap();
                let mut writer = encoder.write_header().unwrap();
                writer.write_image_data(&[1, 2, 3, 255]).unwrap();
                // The encoder supplies a real CRC, but this chunk is not reflected
                // in acTL: a frame-count-only check must not silently accept it.
                writer.write_chunk(kind, &[0; 26]).unwrap();
                writer.finish().unwrap();
            }
            assert_eq!(png_to_dibv5(&bytes), Err(ClipboardError::Invalid));
        }
    }
}
