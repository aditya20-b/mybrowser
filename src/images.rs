//! Image decoding: PNG (`png` crate), JPEG (`jpeg-decoder`) and GIF (`gif`, the
//! first frame). The format is sniffed from the bytes, never from a file name or
//! Content-Type, and every decoder checks the claimed size before allocating.
use std::io::Cursor;
use std::num::NonZeroU64;

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// Straight (non-premultiplied) RGBA, 8 bits per channel, row-major.
    pub rgba: Vec<u8>,
}

impl Image {
    /// Memory the decoded pixels occupy.
    pub fn bytes(&self) -> u64 {
        self.rgba.len() as u64
    }
}

/// Refuse to decode anything with more pixels than this (a decompression-bomb
/// guard: 16M pixels = 64 MB of RGBA; a tiny file can claim far more).
pub const MAX_PIXELS: u64 = 16 * 1024 * 1024;

/// The JPEG decoder keeps coefficient and plane buffers on top of the output
/// (a large progressive CMYK file needs several times the RGBA size), so its own
/// working memory is capped separately.
const MAX_JPEG_WORKING_BYTES: usize = 192 * 1024 * 1024;

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Is a `w` x `h` image acceptable to decode?
fn size_ok(w: u32, h: u32) -> bool {
    w != 0 && h != 0 && (w as u64) * (h as u64) <= MAX_PIXELS
}

/// Decode an image from its bytes, sniffing the format from the content.
pub fn decode(bytes: &[u8]) -> Option<Image> {
    if bytes.starts_with(PNG_SIGNATURE) {
        decode_png(bytes)
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        decode_jpeg(bytes)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        decode_gif(bytes)
    } else {
        None
    }
}

fn decode_png(bytes: &[u8]) -> Option<Image> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    // Expand palettes/low bit depths and strip 16-bit so we only see 8-bit Gray/GrayA/RGB/RGBA.
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if !size_ok(w, h) {
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

fn decode_jpeg(bytes: &[u8]) -> Option<Image> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(bytes));
    decoder.set_max_decoding_buffer_size(MAX_JPEG_WORKING_BYTES);
    decoder.read_info().ok()?; // reads the header only
    let info = decoder.info()?;
    let (w, h) = (info.width as u32, info.height as u32);
    if !size_ok(w, h) {
        return None;
    }
    let data = decoder.decode().ok()?;
    let n = (w * h) as usize;
    let mut rgba = Vec::with_capacity(n * 4);
    match info.pixel_format {
        jpeg_decoder::PixelFormat::L8 => {
            for &g in data.get(..n)? {
                rgba.extend_from_slice(&[g, g, g, 255]);
            }
        }
        jpeg_decoder::PixelFormat::L16 => {
            // the decoder writes 16-bit samples in the machine's byte order: keep the high byte
            for px in data.get(..n * 2)?.chunks_exact(2) {
                let high = (u16::from_ne_bytes([px[0], px[1]]) >> 8) as u8;
                rgba.extend_from_slice(&[high, high, high, 255]);
            }
        }
        jpeg_decoder::PixelFormat::RGB24 => {
            for px in data.get(..n * 3)?.chunks_exact(3) {
                rgba.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
        }
        jpeg_decoder::PixelFormat::CMYK32 => {
            // The decoder hands back the amount of ink (0 = none, the Adobe convention
            // after it undoes their inversion), so light = (1 - c) * (1 - k).
            for px in data.get(..n * 4)?.chunks_exact(4) {
                let light_left = 255 - px[3] as u32;
                let mix = |c: u8| ((255 - c as u32) * light_left / 255) as u8;
                rgba.extend_from_slice(&[mix(px[0]), mix(px[1]), mix(px[2]), 255]);
            }
        }
    }
    Some(Image { w, h, rgba })
}

/// The first frame of a GIF, composited onto a transparent canvas the size of its
/// logical screen (a frame may cover only part of it, or even lie partly or wholly
/// outside it). Animation isn't played.
fn decode_gif(bytes: &[u8]) -> Option<Image> {
    let mut options = gif::DecodeOptions::new();
    options.set_color_output(gif::ColorOutput::RGBA);
    // a frame bigger than the whole-image limit is refused before its buffer is allocated
    options.set_memory_limit(gif::MemoryLimit::Bytes(NonZeroU64::new(MAX_PIXELS * 4)?));
    let mut decoder = options.read_info(Cursor::new(bytes)).ok()?;
    let (w, h) = (decoder.width() as u32, decoder.height() as u32);
    if !size_ok(w, h) {
        return None;
    }
    let frame = decoder.read_next_frame().ok()??;
    let (left, top) = (frame.left as usize, frame.top as usize);
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let pixels = frame.buffer.get(..fw * fh * 4)?;
    let (cw, ch) = (w as usize, h as usize);
    let mut rgba = vec![0u8; cw * ch * 4];
    // the part of each frame row that lands on the canvas (none if it starts past the right edge)
    let copy = fw.min(cw.saturating_sub(left));
    if copy > 0 {
        for row in 0..fh {
            let y = top + row;
            if y >= ch {
                break;
            }
            let src = &pixels[row * fw * 4..row * fw * 4 + copy * 4];
            rgba[(y * cw + left) * 4..(y * cw + left + copy) * 4].copy_from_slice(src);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(data).unwrap();
        out
    }

    fn jpeg(w: u16, h: u16, color: jpeg_encoder::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        jpeg_encoder::Encoder::new(&mut out, 95).encode(data, w, h, color).unwrap();
        out
    }

    /// A GIF with a 4x3 logical screen, a 2-colour palette and one frame at (`left`, `top`).
    fn gif_bytes(frame: (u16, u16, u16, u16), indices: &[u8]) -> Vec<u8> {
        let mut out = vec![];
        {
            let palette = [255, 0, 0, 0, 0, 255]; // index 0 red, 1 blue
            let mut enc = gif::Encoder::new(&mut out, 4, 3, &palette).unwrap();
            let mut f = gif::Frame::default();
            (f.left, f.top, f.width, f.height) = frame;
            f.buffer = std::borrow::Cow::Borrowed(indices);
            enc.write_frame(&f).unwrap();
        }
        out
    }

    fn pixel(img: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.w + x) * 4) as usize;
        [img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]]
    }

    /// JPEG is lossy: is `got` within a few levels of `want` (and opaque)?
    fn close(got: [u8; 4], want: [u8; 3]) -> bool {
        got[3] == 255 && (0..3).all(|k| (got[k] as i32 - want[k] as i32).abs() <= 12)
    }

    #[test]
    fn decodes_each_color_type_to_rgba() {
        let rgba = decode(&encode(1, 1, png::ColorType::Rgba, &[1, 2, 3, 4])).unwrap();
        assert_eq!(rgba.rgba, [1, 2, 3, 4]);
        let rgb = decode(&encode(1, 1, png::ColorType::Rgb, &[1, 2, 3])).unwrap();
        assert_eq!(rgb.rgba, [1, 2, 3, 255]);
        let gray = decode(&encode(2, 1, png::ColorType::Grayscale, &[9, 200])).unwrap();
        assert_eq!(gray.rgba, [9, 9, 9, 255, 200, 200, 200, 255]);
        let ga = decode(&encode(1, 1, png::ColorType::GrayscaleAlpha, &[7, 128])).unwrap();
        assert_eq!(ga.rgba, [7, 7, 7, 128]);
    }

    #[test]
    fn decodes_jpeg_colour_and_grayscale() {
        let red = jpeg(16, 8, jpeg_encoder::ColorType::Rgb, &[200, 30, 30].repeat(16 * 8));
        let img = decode(&red).expect("a JPEG decodes");
        assert_eq!((img.w, img.h), (16, 8));
        assert!(close(pixel(&img, 8, 4), [200, 30, 30]), "{:?}", pixel(&img, 8, 4));
        let gray = jpeg(8, 8, jpeg_encoder::ColorType::Luma, &[120u8; 64]);
        let img = decode(&gray).expect("a grayscale JPEG decodes");
        let p = pixel(&img, 3, 3);
        assert!(close(p, [120, 120, 120]), "{p:?}");
    }

    #[test]
    fn cmyk_jpegs_are_not_black() {
        // every CMYK JPEG used to decode (near) black: the amounts of ink were multiplied
        // instead of subtracted from white
        for color in [jpeg_encoder::ColorType::Cmyk, jpeg_encoder::ColorType::CmykAsYcck] {
            let paper = decode(&jpeg(8, 8, color, &[0, 0, 0, 0].repeat(64))).expect("CMYK decodes");
            assert!(close(pixel(&paper, 4, 4), [255, 255, 255]), "no ink is white paper: {:?}", pixel(&paper, 4, 4));
            let cyan = decode(&jpeg(8, 8, color, &[255, 0, 0, 0].repeat(64))).unwrap();
            assert!(close(pixel(&cyan, 4, 4), [0, 255, 255]), "full cyan: {:?}", pixel(&cyan, 4, 4));
            let black = decode(&jpeg(8, 8, color, &[0, 0, 0, 255].repeat(64))).unwrap();
            assert!(close(pixel(&black, 4, 4), [0, 0, 0]), "full key: {:?}", pixel(&black, 4, 4));
            let half = decode(&jpeg(8, 8, color, &[0, 0, 0, 128].repeat(64))).unwrap();
            assert!(close(pixel(&half, 4, 4), [127, 127, 127]), "half key: {:?}", pixel(&half, 4, 4));
        }
    }

    #[test]
    fn decodes_the_first_frame_of_a_gif_onto_its_logical_screen() {
        // a full-screen frame: row 0 red, rows 1-2 blue
        let full = gif_bytes((0, 0, 4, 3), &[0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1]);
        let img = decode(&full).expect("a GIF decodes");
        assert_eq!((img.w, img.h), (4, 3));
        assert_eq!(pixel(&img, 0, 0), [255, 0, 0, 255]);
        assert_eq!(pixel(&img, 3, 2), [0, 0, 255, 255]);
        // a frame covering part of the screen leaves the rest transparent
        let part = gif_bytes((1, 1, 2, 1), &[1, 1]);
        let img = decode(&part).unwrap();
        assert_eq!(pixel(&img, 0, 0), [0, 0, 0, 0], "outside the frame");
        assert_eq!(pixel(&img, 1, 1), [0, 0, 255, 255]);
        assert_eq!(pixel(&img, 2, 1), [0, 0, 255, 255]);
        assert_eq!(pixel(&img, 3, 1), [0, 0, 0, 0]);
    }

    #[test]
    fn a_gif_frame_outside_its_screen_is_clipped_not_a_panic() {
        // a frame starting past the right edge panicked on the last rows (an empty slice
        // still has to start inside the buffer), and took the whole page down with it
        let blank = |img: &Image| img.rgba.iter().all(|&b| b == 0);
        let right = decode(&gif_bytes((10, 2, 1, 1), &[0])).expect("decodes to an empty canvas");
        assert_eq!((right.w, right.h), (4, 3));
        assert!(blank(&right), "nothing of the frame is on screen");
        let below = decode(&gif_bytes((0, 7, 2, 2), &[0; 4])).unwrap();
        assert!(blank(&below), "a frame below the screen");
        // partly outside: the visible part is kept
        let edge = decode(&gif_bytes((3, 2, 3, 3), &[1; 9])).unwrap();
        assert_eq!(pixel(&edge, 3, 2), [0, 0, 255, 255]);
        assert_eq!(pixel(&edge, 2, 2), [0, 0, 0, 0]);
        // every offset around and beyond the edges
        for left in [0u16, 1, 3, 4, 5, 100, 65535] {
            for top in [0u16, 1, 2, 3, 4, 100, 65535] {
                let _ = decode(&gif_bytes((left, top, 2, 2), &[0; 4]));
            }
        }
    }

    #[test]
    fn rejects_garbage_truncated_and_unknown_formats() {
        assert!(decode(b"").is_none());
        assert!(decode(b"RIFF....WEBPVP8 ").is_none(), "formats we can't decode");
        assert!(decode(b"\x89PNG\r\n\x1a\nnot really").is_none());
        assert!(decode(b"\xFF\xD8\xFFnot really").is_none());
        assert!(decode(b"GIF89a not really").is_none());
        let good = encode(4, 4, png::ColorType::Rgb, &[0; 48]);
        assert!(decode(&good[..good.len() / 2]).is_none());
        // truncated JPEGs and GIFs must never panic (what they decode to is up to the decoder)
        let j = jpeg(16, 16, jpeg_encoder::ColorType::Rgb, &[90u8; 16 * 16 * 3]);
        for cut in [4, 20, j.len() / 2, j.len() - 1] {
            let _ = decode(&j[..cut]);
        }
        let g = gif_bytes((0, 0, 4, 3), &[0; 12]);
        for cut in [6, 10, g.len() - 8, g.len() - 1] {
            let _ = decode(&g[..cut]);
        }
        assert!(decode(&g[..10]).is_none());
    }

    #[test]
    fn oversized_images_are_refused_before_allocating() {
        // 5000 x 4000 = 20M pixels (> MAX_PIXELS) compresses to a few KB but would need 80 MB
        let big = encode(5000, 4000, png::ColorType::Grayscale, &vec![0u8; 5000 * 4000]);
        assert!(big.len() < 100_000, "the file is small ({} bytes)", big.len());
        assert!(decode(&big).is_none());
        let ok = encode(100, 100, png::ColorType::Grayscale, &vec![0u8; 100 * 100]);
        assert_eq!(decode(&ok).unwrap().bytes(), 100 * 100 * 4);
        // the same for JPEG (a 5000 x 4000 flat grey JPEG is a few hundred KB) and GIF (a 65535 x 65535 screen)
        let big_jpeg = jpeg(5000, 4000, jpeg_encoder::ColorType::Luma, &vec![128u8; 5000 * 4000]);
        assert!(big_jpeg.len() < 2_000_000, "{} bytes", big_jpeg.len());
        assert!(decode(&big_jpeg).is_none());
        let mut huge_gif = gif_bytes((0, 0, 4, 3), &[0; 12]);
        huge_gif[6..10].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]); // logical screen width and height
        assert!(decode(&huge_gif).is_none());
    }

    #[test]
    fn bilinear_sampling_stays_inside_the_image() {
        let img = decode(&encode(2, 1, png::ColorType::Grayscale, &[0, 200])).unwrap();
        for (x, y) in [(-5.0, -5.0), (0.0, 0.0), (1.0, 0.5), (2.0, 1.0), (50.0, 50.0), (f32::MAX, f32::MAX)] {
            let [r, _, _, a] = img.sample(x, y);
            assert!((0.0..=200.0).contains(&r) && a == 255.0, "{x},{y} -> {r}");
        }
        assert!((img.sample(1.0, 0.5)[0] - 100.0).abs() < 1.0, "halfway between the two pixels");
    }
}
