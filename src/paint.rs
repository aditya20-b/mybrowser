//! Stage 5: display list -> pixels. A tiny software rasterizer.
use crate::css::Color;
use crate::fonts::{invisible, Fonts};
use crate::layout::{Item, Layout};
use std::collections::HashMap;

/// Glyph bitmaps are cached per (face, char, pixel size). Cap the cache so
/// zooming through many sizes (or pathological text) can't grow it forever.
const MAX_CACHED_GLYPHS: usize = 4096;

#[derive(Default)]
pub struct Painter {
    glyphs: HashMap<(usize, char, u32), (fontdue::Metrics, Vec<u8>)>,
}

fn blend(dst: u32, c: Color, a: u32) -> u32 {
    let mix = |d: u32, s: u8| (d * (255 - a) + s as u32 * a + 127) / 255;
    mix((dst >> 16) & 255, c.r) << 16 | mix((dst >> 8) & 255, c.g) << 8 | mix(dst & 255, c.b)
}

impl Painter {
    /// Paint `layout` into `buf` (0x00RRGGBB, `w` x `h` device pixels).
    /// `scroll` is in layout (logical) pixels; `scale` is device pixels per logical pixel.
    pub fn paint(&mut self, buf: &mut [u32], w: usize, h: usize, layout: &Layout, fonts: &Fonts, scroll: f32, scale: f32) {
        // A zero, negative or NaN scale would divide by zero or flip the page.
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        if self.glyphs.len() > MAX_CACHED_GLYPHS {
            self.glyphs.clear();
        }
        buf.fill(layout.canvas_bg.unwrap_or(Color::WHITE).to_u32());
        let view_h = h as f32 / scale;

        for item in &layout.items {
            match item {
                Item::Nop => {}
                Item::Rect { x, y, w: rw, h: rh, color } => {
                    if y + rh < scroll || *y > scroll + view_h {
                        continue;
                    }
                    // `as i32` saturates for huge values, so the +1 must saturate too.
                    let x0 = (x * scale).round() as i32;
                    let y0 = ((y - scroll) * scale).round() as i32;
                    let x1 = (((x + rw) * scale).round() as i32).max(x0.saturating_add(1));
                    let y1 = (((y - scroll + rh) * scale).round() as i32).max(y0.saturating_add(1));
                    fill_rect(buf, w, h, x0, y0, x1, y1, *color);
                }
                Item::Image { x, y, w: iw, h: ih, img } => {
                    if y + ih < scroll || *y > scroll + view_h {
                        continue;
                    }
                    let x0 = (x * scale).round() as i32;
                    let y0 = ((y - scroll) * scale).round() as i32;
                    let dw = ((iw * scale).round() as i32).max(1);
                    let dh = ((ih * scale).round() as i32).max(1);
                    draw_image(buf, w, h, (x0, y0, dw, dh), img);
                }
                Item::Text { x, baseline, text, size, face, color } => {
                    let by = (baseline - scroll) * scale;
                    if by < -size * scale * 2.0 || by > h as f32 + size * scale {
                        continue;
                    }
                    let px = size * scale;
                    let fi = face.index();
                    let mut pen = x * scale;
                    for ch in text.chars().filter(|c| !invisible(*c)) {
                        let (m, bmp) = self
                            .glyphs
                            .entry((fi, ch, px.to_bits()))
                            .or_insert_with(|| fonts.font(*face).rasterize(ch, px));
                        let gx = (pen + m.xmin as f32).round() as i32;
                        let gy = (by - m.ymin as f32 - m.height as f32).round() as i32;
                        for row in 0..m.height {
                            let yy = gy.saturating_add(row as i32);
                            if yy < 0 || yy >= h as i32 {
                                continue;
                            }
                            for col in 0..m.width {
                                let xx = gx.saturating_add(col as i32);
                                let cov = bmp[row * m.width + col] as u32;
                                if cov == 0 || xx < 0 || xx >= w as i32 {
                                    continue;
                                }
                                let p = &mut buf[yy as usize * w + xx as usize];
                                *p = blend(*p, *color, cov);
                            }
                        }
                        pen += m.advance_width;
                    }
                }
            }
        }

        // scrollbar thumb
        if layout.height > view_h {
            let track = h as f32;
            let thumb_h = (track * view_h / layout.height).max(24.0);
            let max_scroll = layout.height - view_h;
            let thumb_y = (track - thumb_h) * (scroll / max_scroll).clamp(0.0, 1.0);
            let bar_w = (8.0 * scale) as i32;
            fill_rect(buf, w, h, w as i32 - bar_w - 2, thumb_y as i32, w as i32 - 2, (thumb_y + thumb_h) as i32,
                Color { r: 150, g: 150, b: 150 });
        }
    }
}

/// Draw `img` scaled into the device-pixel rect `(x0, y0, dw, dh)` with bilinear
/// filtering and alpha blending. Only visible pixels are visited, so a huge
/// destination rect costs no more than the buffer itself.
fn draw_image(buf: &mut [u32], w: usize, h: usize, rect: (i32, i32, i32, i32), img: &crate::images::Image) {
    let (x0, y0, dw, dh) = rect;
    let (cx0, cx1) = (x0.max(0), x0.saturating_add(dw).min(w as i32));
    let (cy0, cy1) = (y0.max(0), y0.saturating_add(dh).min(h as i32));
    let (sx, sy) = (img.w as f32 / dw as f32, img.h as f32 / dh as f32);
    for py in cy0..cy1 {
        let fy = ((py - y0) as f32 + 0.5) * sy;
        for px in cx0..cx1 {
            let fx = ((px - x0) as f32 + 0.5) * sx;
            let [r, g, b, a] = img.sample(fx, fy);
            if a < 0.5 {
                continue;
            }
            let c = Color { r: r as u8, g: g as u8, b: b as u8 };
            let p = &mut buf[py as usize * w + px as usize];
            *p = if a >= 254.5 { c.to_u32() } else { blend(*p, c, a as u32) };
        }
    }
}

fn fill_rect(buf: &mut [u32], w: usize, h: usize, x0: i32, y0: i32, x1: i32, y1: i32, color: Color) {
    let (x0, x1) = (x0.clamp(0, w as i32), x1.clamp(0, w as i32));
    let (y0, y1) = (y0.clamp(0, h as i32), y1.clamp(0, h as i32));
    if x1 <= x0 || y1 <= y0 {
        return; // empty after clipping (slicing with start > end would panic)
    }
    let c = color.to_u32();
    for y in y0..y1 {
        buf[y as usize * w + x0 as usize..y as usize * w + x1 as usize].fill(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use url::Url;

    fn page_url() -> Url {
        Url::parse("file:///tmp/t.html").unwrap()
    }

    fn paint_page(src: &str, w: usize, h: usize, scroll: f32, scale: f32) -> Vec<u32> {
        let fonts = Fonts::load();
        let mut page = Page::from_source(&page_url(), src);
        page.relayout(&fonts, 300.0);
        let mut buf = vec![0u32; w * h];
        Painter::default().paint(&mut buf, w, h, &page.layout, &fonts, scroll, scale);
        buf
    }

    #[test]
    fn huge_coordinates_do_not_overflow() {
        // `margin-left: 3e9px` saturates the i32 pixel coordinates (panicked in debug builds)
        let src = "<div style='margin-left:3000000000px;width:10px;height:10px;background:red'>x</div>\
                   <div style='margin-top:3000000000px;height:10px;background:blue'>y</div>";
        paint_page(src, 100, 100, 0.0, 1.0);
        paint_page(src, 100, 100, 1e9, 2.0);
    }

    #[test]
    fn nonsense_scales_and_tiny_buffers_are_survivable() {
        let src = "<p style='background:#eee'>hello <a href=x>world</a></p><hr>";
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY, 0.5, 3.0] {
            paint_page(src, 50, 40, 0.0, scale);
        }
        for (w, h) in [(1, 1), (3, 1), (1, 7)] {
            paint_page(src, w, h, 0.0, 1.0);
        }
    }

    #[test]
    fn zero_width_characters_paint_nothing() {
        let plain = paint_page("<p>ab</p>", 120, 40, 0.0, 1.0);
        let with = paint_page("<p>a&#x200B;&#x200D;&#xAD;&#xFE0F;b</p>", 120, 40, 0.0, 1.0);
        assert_eq!(plain, with);
    }

    #[test]
    fn glyph_cache_stays_bounded() {
        let fonts = Fonts::load();
        let mut page = Page::from_source(&page_url(), "<p>abcdefghijklmnopqrstuvwxyz</p>");
        page.relayout(&fonts, 300.0);
        let mut painter = Painter::default();
        let mut buf = vec![0u32; 200 * 60];
        // many distinct sizes would otherwise add 26 glyphs each
        for i in 0..400 {
            painter.paint(&mut buf, 200, 60, &page.layout, &fonts, 0.0, 1.0 + i as f32 * 0.01);
        }
        assert!(painter.glyphs.len() <= MAX_CACHED_GLYPHS + 64, "{}", painter.glyphs.len());
    }
}
