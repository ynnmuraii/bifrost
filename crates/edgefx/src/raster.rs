//! A 5x7 ASCII bitmap font and a premultiplied-ARGB canvas. The same pixels go to a Win32
//! layered window, a Wayland `wl_shm` buffer and the offscreen PNG screenshots.

/// `0xRRGGBB`. Tokens of the dark theme in `mdf.css`: `--accent`.
pub const ACCENT: u32 = 0xFF_5A_2E;
/// `mdf.css` dark `--ink-2`.
pub const DIM: u32 = 0x9C_9A_92;

/// Rows of 5 bits, most significant bit on the left.
/// Order: `. : + * # / \ - | = ~ < >` (13 kinds, `sim::GLYPH_KINDS`).
const GLYPHS: [[u8; 7]; 13] = [
    [0, 0, 0, 0, 0, 0b01100, 0b01100],
    [0, 0b01100, 0b01100, 0, 0b01100, 0b01100, 0],
    [0, 0b00100, 0b00100, 0b11111, 0b00100, 0b00100, 0],
    [0, 0b10101, 0b01110, 0b11111, 0b01110, 0b10101, 0],
    [
        0b01010, 0b01010, 0b11111, 0b01010, 0b11111, 0b01010, 0b01010,
    ],
    [
        0b00001, 0b00010, 0b00010, 0b00100, 0b01000, 0b01000, 0b10000,
    ],
    [
        0b10000, 0b01000, 0b01000, 0b00100, 0b00010, 0b00010, 0b00001,
    ],
    [0, 0, 0, 0b11111, 0, 0, 0],
    [0b00100; 7],
    [0, 0, 0b11111, 0, 0b11111, 0, 0],
    [0, 0, 0b01000, 0b10101, 0b00010, 0, 0],
    [
        0b00010, 0b00100, 0b01000, 0b10000, 0b01000, 0b00100, 0b00010,
    ],
    [
        0b01000, 0b00100, 0b00010, 0b00001, 0b00010, 0b00100, 0b01000,
    ],
];

/// A borrowed pixel buffer: `width * height` premultiplied `0xAARRGGBB` words.
pub struct Canvas<'a> {
    px: &'a mut [u32],
    w: usize,
    h: usize,
}

impl<'a> Canvas<'a> {
    /// # Panics
    /// If the buffer is smaller than `w * h`.
    pub fn new(px: &'a mut [u32], w: usize, h: usize) -> Self {
        assert!(px.len() >= w * h, "canvas buffer too small");
        Self { px, w, h }
    }

    #[must_use]
    pub const fn width(&self) -> usize {
        self.w
    }

    #[must_use]
    pub const fn height(&self) -> usize {
        self.h
    }

    pub fn clear(&mut self) {
        self.px[..self.w * self.h].fill(0);
    }

    /// Draws glyph `kind` with its top-left at `(x, y)`, each font pixel `cell` px square,
    /// `rgb` at `alpha` (`0..=1`). Clipped to the canvas.
    pub fn glyph(&mut self, kind: u8, x: i32, y: i32, cell: i32, rgb: u32, alpha: f32) {
        let rows = &GLYPHS[usize::from(kind) % GLYPHS.len()];
        let a = (alpha.clamp(0.0, 1.0) * 255.0) as u32;
        let channel = |shift: u32| (((rgb >> shift) & 0xFF) * a / 255) & 0xFF;
        let src = (a << 24) | (channel(16) << 16) | (channel(8) << 8) | channel(0);
        let inv = 255 - a;
        for (row, bits) in rows.iter().enumerate() {
            for col in 0..5 {
                if bits >> (4 - col) & 1 == 0 {
                    continue;
                }
                for dy in 0..cell {
                    let py = y + row as i32 * cell + dy;
                    if py < 0 || py >= self.h as i32 {
                        continue;
                    }
                    for dx in 0..cell {
                        let pxx = x + col * cell + dx;
                        if pxx < 0 || pxx >= self.w as i32 {
                            continue;
                        }
                        let dst = &mut self.px[py as usize * self.w + pxx as usize];
                        *dst = blend(src, *dst, inv);
                    }
                }
            }
        }
    }
}

/// `src + dst * (1 - a)` per channel, premultiplied.
fn blend(src: u32, dst: u32, inv: u32) -> u32 {
    let mix = |shift: u32| {
        let s = (src >> shift) & 0xFF;
        let d = (dst >> shift) & 0xFF;
        (s + d * inv / 255).min(255) << shift
    };
    mix(24) | mix(16) | mix(8) | mix(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_glyph_lands_where_asked_and_clips_at_the_border() {
        let mut buf = vec![0u32; 20 * 20];
        let mut canvas = Canvas::new(&mut buf, 20, 20);
        canvas.glyph(8, 5, 2, 1, ACCENT, 1.0); // '|': the middle column, 7 rows
        for row in 2..9 {
            assert_eq!(buf[row * 20 + 7], 0xFF_FF_5A_2E, "row {row}");
        }
        assert_eq!(buf[2 * 20 + 6], 0);
        let mut canvas = Canvas::new(&mut buf, 20, 20);
        canvas.clear();
        canvas.glyph(4, -3, -3, 3, DIM, 1.0);
        canvas.glyph(4, 18, 18, 3, DIM, 1.0);
    }

    #[test]
    fn alpha_is_premultiplied() {
        let mut buf = vec![0u32; 4];
        Canvas::new(&mut buf, 2, 2).glyph(7, 0, -3, 1, 0xFF_FF_FF, 0.5);
        let lit = buf.iter().copied().find(|p| *p != 0).expect("a lit pixel");
        assert_eq!(lit >> 24, 127);
        assert_eq!((lit >> 16) & 0xFF, 127);
    }
}
