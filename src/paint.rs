//! Stage 5: display list -> pixels. A tiny software rasterizer.
use crate::css::Color;
use crate::fonts::Fonts;
use crate::layout::{Item, Layout};
use std::collections::HashMap;

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
        buf.fill(layout.canvas_bg.unwrap_or(Color::WHITE).to_u32());
        let view_h = h as f32 / scale;

        for item in &layout.items {
            match item {
                Item::Nop => {}
                Item::Rect { x, y, w: rw, h: rh, color } => {
                    if y + rh < scroll || *y > scroll + view_h {
                        continue;
                    }
                    let x0 = (x * scale).round() as i32;
                    let y0 = ((y - scroll) * scale).round() as i32;
                    let x1 = (((x + rw) * scale).round() as i32).max(x0 + 1);
                    let y1 = (((y - scroll + rh) * scale).round() as i32).max(y0 + 1);
                    fill_rect(buf, w, h, x0, y0, x1, y1, *color);
                }
                Item::Text { x, baseline, text, size, face, color } => {
                    let by = (baseline - scroll) * scale;
                    if by < -size * scale * 2.0 || by > h as f32 + size * scale {
                        continue;
                    }
                    let px = size * scale;
                    let fi = face.index();
                    let mut pen = x * scale;
                    for ch in text.chars() {
                        let (m, bmp) = self
                            .glyphs
                            .entry((fi, ch, px.to_bits()))
                            .or_insert_with(|| fonts.font(*face).rasterize(ch, px));
                        let gx = (pen + m.xmin as f32).round() as i32;
                        let gy = (by - m.ymin as f32 - m.height as f32).round() as i32;
                        for row in 0..m.height {
                            let yy = gy + row as i32;
                            if yy < 0 || yy >= h as i32 {
                                continue;
                            }
                            for col in 0..m.width {
                                let xx = gx + col as i32;
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
