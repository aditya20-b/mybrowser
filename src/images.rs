//! Image decoding (PNG only for now, via the `png` crate).
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// Straight (non-premultiplied) RGBA, 8 bits per channel, row-major.
    pub rgba: Vec<u8>,
}

/// Refuse to decode anything that would need more than this many pixels
/// (a decompression-bomb guard: 64M pixels = 256 MB of RGBA).
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

pub fn load_png(path: &Path) -> Option<Image> {
    let file = File::open(path).ok()?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    // Expand palettes/low bit depths and strip 16-bit so we only see 8-bit Gray/GrayA/RGB/RGBA.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > MAX_PIXELS {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buf).ok()?;
    let data = &buf[..frame.buffer_size()];

    let n = (w * h) as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    match frame.color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(data.get(..n * 4)?),
        png::ColorType::Rgb => {
            for px in data.get(..n * 3)?.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in data.get(..n)? {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for px in data.get(..n * 2)?.chunks_exact(2) {
                rgba.extend_from_slice(&[px[0], px[0], px[0], px[1]]);
            }
        }
        png::ColorType::Indexed => return None, // normalize_to_color8 expands these
    }
    Some(Image { w, h, rgba })
}

impl Image {
    /// Bilinear sample at continuous pixel coordinates (texel centres at +0.5).
    pub fn sample(&self, fx: f32, fy: f32) -> [f32; 4] {
        let x = (fx - 0.5).clamp(0.0, (self.w - 1) as f32);
        let y = (fy - 0.5).clamp(0.0, (self.h - 1) as f32);
        let (x0, y0) = (x.floor() as u32, y.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
        let (tx, ty) = (x - x0 as f32, y - y0 as f32);
        let px = |xx: u32, yy: u32| {
            let i = ((yy * self.w + xx) * 4) as usize;
            [self.rgba[i] as f32, self.rgba[i + 1] as f32, self.rgba[i + 2] as f32, self.rgba[i + 3] as f32]
        };
        let (a, b, c, d) = (px(x0, y0), px(x1, y0), px(x0, y1), px(x1, y1));
        let mut out = [0.0; 4];
        for k in 0..4 {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bot = c[k] + (d[k] - c[k]) * tx;
            out[k] = top + (bot - top) * ty;
        }
        out
    }
}
