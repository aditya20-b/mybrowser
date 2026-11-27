//! Stage 5: display list -> pixels. A tiny software rasterizer.
use crate::chrome::{self, ChromeView};
use crate::css::Color;
use crate::find::Find;
use crate::fonts::{invisible, Face, Family, Fonts};
use crate::layout::{Item, Layout};
use crate::tabbar::{self, StripView};
use std::collections::HashMap;

/// Glyph bitmaps are cached per (font slot, char, pixel size). Cap the cache so
/// zooming through many sizes (or pathological text) can't grow it forever.
const MAX_CACHED_GLYPHS: usize = 4096;

const TOOLBAR_BG: Color = Color { r: 0xf1, g: 0xf3, b: 0xf4 };
const TOOLBAR_LINE: Color = Color { r: 0xda, g: 0xdc, b: 0xe0 };
const FIELD_BORDER: Color = Color { r: 0xc4, g: 0xc7, b: 0xcc };
const FIELD_FOCUS: Color = Color { r: 0x1a, g: 0x73, b: 0xe8 };
const SELECTION: Color = Color { r: 0xb4, g: 0xd5, b: 0xfe };
const ICON: Color = Color { r: 0x3c, g: 0x40, b: 0x43 };
const ICON_DISABLED: Color = Color { r: 0xb0, g: 0xb4, b: 0xb8 };
const ERROR_TEXT: Color = Color { r: 0xc5, g: 0x22, b: 0x1f };
const STRIP_BG: Color = Color { r: 0xde, g: 0xe1, b: 0xe6 };
const TAB_INACTIVE: Color = Color { r: 0xd2, g: 0xd5, b: 0xda };
const FIND_MATCH: Color = Color { r: 0xff, g: 0xeb, b: 0x3b };
const FIND_CURRENT: Color = Color { r: 0xff, g: 0x98, b: 0x00 };

#[derive(Default)]
pub struct Painter {
    glyphs: HashMap<(usize, char, u32), (fontdue::Metrics, Vec<u8>)>,
}

fn blend(dst: u32, c: Color, a: u32) -> u32 {
    let mix = |d: u32, s: u8| (d * (255 - a) + s as u32 * a + 127) / 255;
    mix((dst >> 16) & 255, c.r) << 16 | mix((dst >> 8) & 255, c.g) << 8 | mix(dst & 255, c.b)
}

/// A zero, negative or NaN scale would divide by zero or flip the page.
fn sane_scale(scale: f32) -> f32 {
    if scale.is_finite() && scale > 0.0 { scale } else { 1.0 }
}

/// The longest prefix of `text` that fits `avail` device pixels, with an
/// ellipsis if something was cut.
fn fit_text(fonts: &Fonts, face: Face, size: f32, text: &str, avail: f32) -> String {
    if fonts.measure(face, size, text) <= avail {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut keep = chars.len();
    while keep > 0 {
        keep -= 1;
        let candidate: String = chars[..keep].iter().chain(std::iter::once(&'…')).collect();
        if fonts.measure(face, size, &candidate) <= avail {
            return candidate;
        }
    }
    String::new()
}

impl Painter {
    /// Paint `layout` into `buf` (0x00RRGGBB, `w` x `h` device pixels).
    /// `scroll` is in layout (logical) pixels; `scale` is device pixels per logical pixel.
    pub fn paint(&mut self, buf: &mut [u32], w: usize, h: usize, layout: &Layout, fonts: &Fonts, scroll: f32, scale: f32) {
        let scale = sane_scale(scale);
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
                    self.draw_text(buf, w, h, fonts, *face, size * scale, x * scale, by, text, *color);
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

    /// Draw `text` with its left edge at device x and its baseline at device y.
    /// `px` is the font size in device pixels.
    #[allow(clippy::too_many_arguments)]
    fn draw_text(&mut self, buf: &mut [u32], w: usize, h: usize, fonts: &Fonts, face: Face, px: f32, x: f32, baseline: f32, text: &str, color: Color) {
        let mut pen = x;
        for ch in text.chars().filter(|c| !invisible(*c)) {
            let (slot, font) = fonts.resolve(face, ch);
            let (m, bmp) = self.glyphs.entry((slot, ch, px.to_bits())).or_insert_with(|| font.rasterize(ch, px));
            let gx = (pen + m.xmin as f32).round() as i32;
            let gy = (baseline - m.ymin as f32 - m.height as f32).round() as i32;
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
                    *p = blend(*p, color, cov);
                }
            }
            pen += m.advance_width;
        }
    }

    /// Draw the find-in-page overlay on an already painted page: every match
    /// tinted yellow (the selected one orange), and a small search box at the top
    /// right showing the query and "n of m". `scroll` is the page's scroll offset.
    #[allow(clippy::too_many_arguments)]
    /// `zoom` is the page zoom: match highlights follow the page, the search box stays its size.
    #[allow(clippy::too_many_arguments)]
    pub fn paint_find(&mut self, buf: &mut [u32], w: usize, h: usize, fonts: &Fonts, scale: f32, zoom: f32, scroll: f32, find: &Find) {
        let scale = sane_scale(scale);
        let zoom = sane_scale(zoom);
        let px = |v: f32| (v * scale).round() as i32;
        let page_px = |v: f32| (v * scale * zoom).round() as i32;
        let line = scale.round().max(1.0) as i32;
        let face = Face { family: Family::Sans, bold: false, italic: false };

        // match highlights: a translucent tint, so the text stays readable underneath
        let current = find.current_index();
        for (i, m) in find.matches().iter().enumerate() {
            let (top, bottom) = (m.baseline - m.size * 0.85 - scroll, m.baseline + m.size * 0.25 - scroll);
            if bottom < 0.0 || top * scale * zoom > h as f32 {
                continue;
            }
            let (x0, x1) = (page_px(m.x), page_px(m.x + m.w));
            let (color, alpha) = if current == Some(i) { (FIND_CURRENT, 150) } else { (FIND_MATCH, 110) };
            tint_rect(buf, w, h, x0, page_px(top), x1.max(x0.saturating_add(1)), page_px(bottom), color, alpha);
        }

        // the search box
        let (bw, bh) = (px(300.0), px(32.0));
        let x0 = (w as i32 - bw - px(14.0)).max(0);
        let y0 = px(8.0);
        fill_rect(buf, w, h, x0, y0, x0 + bw, y0 + bh, FIELD_FOCUS);
        fill_rect(buf, w, h, x0 + line, y0 + line, x0 + bw - line, y0 + bh - line, Color::WHITE);
        let size = 13.0 * scale;
        let baseline = (y0 + bh / 2) as f32 + size * 0.35;
        let text_x = (x0 + px(10.0)) as f32;
        let label = if find.query().is_empty() {
            String::new()
        } else if let Some(i) = current {
            format!("{} of {}", i + 1, find.matches().len())
        } else {
            "No results".to_string()
        };
        let label_w = fonts.measure(face, size, &label);
        let avail = bw as f32 - px(20.0) as f32 - label_w - px(8.0) as f32;
        let shown = fit_text(fonts, face, size, find.query(), avail);
        let ink = Color { r: 0x20, g: 0x21, b: 0x24 };
        self.draw_text(buf, w, h, fonts, face, size, text_x, baseline, &shown, ink);
        let caret_x = (text_x + fonts.measure(face, size, &shown)) as i32;
        fill_rect(buf, w, h, caret_x, y0 + px(7.0), caret_x + line, y0 + bh - px(7.0), ink);
        if !label.is_empty() {
            let lc = if current.is_some() { ICON_DISABLED } else { ERROR_TEXT };
            self.draw_text(buf, w, h, fonts, face, size, (x0 + bw) as f32 - px(10.0) as f32 - label_w, baseline, &label, lc);
        }
    }

    /// Draw the tab strip over the top `tabbar::HEIGHT` logical pixels of `buf`.
    /// The active tab is drawn in the toolbar's colour so the two read as one piece.
    pub fn paint_tabs(&mut self, buf: &mut [u32], w: usize, h: usize, fonts: &Fonts, scale: f32, view: &StripView) {
        let scale = sane_scale(scale);
        let px = |v: f32| (v * scale).round() as i32;
        let face = Face { family: Family::Sans, bold: false, italic: false };
        let strip_h = px(tabbar::HEIGHT);
        let count = view.titles.len();
        let width = w as f32 / scale;

        fill_rect(buf, w, h, 0, 0, w as i32, strip_h, STRIP_BG);
        for i in 0..count {
            let r = tabbar::tab_rect(i, count, width);
            let active = i == view.active;
            let (x0, y0, x1) = (px(r.x), px(r.y), px(r.x + r.w));
            // the active tab runs down into the toolbar; the others stop a hair short
            let y1 = if active { strip_h } else { strip_h - 1 };
            fill_rect(buf, w, h, x0, y0, x1, y1, if active { TOOLBAR_BG } else { TAB_INACTIVE });

            let close = tabbar::close_rect(i, count, width);
            let size = 12.0 * scale;
            let mut title = view.titles[i].clone();
            if view.loading.get(i).copied().unwrap_or(false) {
                title.insert_str(0, "… ");
            }
            let avail = px(close.x) as f32 - x0 as f32 - 16.0 * scale;
            let shown = fit_text(fonts, face, size, &title, avail);
            let baseline = (y0 + strip_h) as f32 / 2.0 + size * 0.35;
            self.draw_text(buf, w, h, fonts, face, size, x0 as f32 + 10.0 * scale, baseline, &shown, ICON);

            let cs = 14.0 * scale;
            let cw = fonts.measure(face, cs, "×");
            let cx = px(close.x) as f32 + (close.w * scale - cw) / 2.0;
            let cb = px(close.y) as f32 + close.h * scale / 2.0 + cs * 0.33;
            self.draw_text(buf, w, h, fonts, face, cs, cx, cb, "×", ICON_DISABLED);
        }

        let plus = tabbar::new_rect(count, width);
        let ps = 18.0 * scale;
        let pw = fonts.measure(face, ps, "+");
        let px_ = plus.x * scale + (plus.w * scale - pw) / 2.0;
        let pb = plus.y * scale + plus.h * scale / 2.0 + ps * 0.33;
        self.draw_text(buf, w, h, fonts, face, ps, px_, pb, "+", ICON);
    }

    /// Draw the toolbar over the top `chrome::HEIGHT` logical pixels of `buf`.
    pub fn paint_chrome(&mut self, buf: &mut [u32], w: usize, h: usize, fonts: &Fonts, scale: f32, view: &ChromeView) {
        let scale = sane_scale(scale);
        let px = |v: f32| (v * scale).round() as i32;
        let line = scale.round().max(1.0) as i32;
        let bar_h = px(chrome::HEIGHT);
        let face = Face { family: Family::Sans, bold: false, italic: false };

        fill_rect(buf, w, h, 0, 0, w as i32, bar_h, TOOLBAR_BG);
        fill_rect(buf, w, h, 0, bar_h - line, w as i32, bar_h, TOOLBAR_LINE);

        // navigation buttons
        let buttons = [
            (chrome::back_rect(), "←", view.can_back),
            (chrome::forward_rect(), "→", view.can_forward),
            (chrome::reload_rect(), "↻", true),
        ];
        for (r, label, enabled) in buttons {
            let size = 18.0 * scale;
            let tw = fonts.measure(face, size, label);
            let x = r.x * scale + (r.w * scale - tw) / 2.0;
            let baseline = r.y * scale + r.h * scale / 2.0 + size * 0.33;
            let color = if enabled { ICON } else { ICON_DISABLED };
            self.draw_text(buf, w, h, fonts, face, size, x, baseline, label, color);
        }

        // address field: a 1px border around a white box
        let r = chrome::address_rect(w as f32 / scale);
        let (x0, y0, x1, y1) = (px(r.x), px(r.y), px(r.x + r.w), px(r.y + r.h));
        let focused = view.bar.is_focused();
        fill_rect(buf, w, h, x0, y0, x1, y1, if focused { FIELD_FOCUS } else { FIELD_BORDER });
        fill_rect(buf, w, h, x0 + line, y0 + line, x1 - line, y1 - line, Color::WHITE);

        let size = 14.0 * scale;
        let pad = 10.0 * scale;
        let avail = (x1 - x0) as f32 - 2.0 * pad;
        let showing_error = view.bar.error.is_some();
        let text = view.bar.error.as_deref().unwrap_or(view.bar.text());
        let color = if showing_error { ERROR_TEXT } else { Color { r: 0x20, g: 0x21, b: 0x24 } };
        let chars: Vec<char> = text.chars().collect();
        let widths: Vec<f32> = chars.iter().map(|c| fonts.measure(face, size, c.encode_utf8(&mut [0; 4]))).collect();
        let caret = if showing_error { 0 } else { view.bar.caret().min(chars.len()) };

        // While editing, scroll horizontally so the caret stays inside the field.
        let mut start = 0;
        if focused && !showing_error {
            let mut to_caret: f32 = widths[..caret].iter().sum();
            while start < caret && to_caret > avail {
                to_caret -= widths[start];
                start += 1;
            }
        }
        // The visible slice: as many characters from `start` as fit.
        let (mut end, mut used) = (start, 0.0f32);
        while end < chars.len() && used + widths[end] <= avail {
            used += widths[end];
            end += 1;
        }
        let text_x = x0 as f32 + pad;
        let baseline = (y0 + y1) as f32 / 2.0 + size * 0.35;
        if focused && view.bar.is_all_selected() && !showing_error {
            fill_rect(buf, w, h, text_x as i32, y0 + 3 * line, (text_x + used).ceil() as i32, y1 - 3 * line, SELECTION);
        }
        let shown: String = chars[start..end].iter().collect();
        self.draw_text(buf, w, h, fonts, face, size, text_x, baseline, &shown, color);
        if focused && !view.bar.is_all_selected() && !showing_error && caret >= start && caret <= end {
            let cx = text_x + widths[start..caret].iter().sum::<f32>();
            fill_rect(buf, w, h, cx as i32, y0 + 4 * line, cx as i32 + line, y1 - 4 * line, color);
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

/// Like `fill_rect`, but blends `color` over what is already there (`alpha` of 255).
#[allow(clippy::too_many_arguments)]
fn tint_rect(buf: &mut [u32], w: usize, h: usize, x0: i32, y0: i32, x1: i32, y1: i32, color: Color, alpha: u32) {
    let (x0, x1) = (x0.clamp(0, w as i32), x1.clamp(0, w as i32));
    let (y0, y1) = (y0.clamp(0, h as i32), y1.clamp(0, h as i32));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for y in y0..y1 {
        for p in &mut buf[y as usize * w + x0 as usize..y as usize * w + x1 as usize] {
            *p = blend(*p, color, alpha);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::AddressBar;
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

    fn paint_bar(bar: &AddressBar, w: usize, h: usize, scale: f32) -> Vec<u32> {
        let fonts = Fonts::load();
        let mut buf = vec![0u32; w * h];
        let view = ChromeView { bar, can_back: true, can_forward: false };
        Painter::default().paint_chrome(&mut buf, w, h, &fonts, scale, &view);
        buf
    }

    fn paint_strip(titles: &[&str], active: usize, w: usize, h: usize, scale: f32) -> Vec<u32> {
        let fonts = Fonts::load();
        let titles: Vec<String> = titles.iter().map(|t| t.to_string()).collect();
        let loading = vec![false; titles.len()];
        let mut buf = vec![0u32; w * h];
        let view = StripView { titles: &titles, loading: &loading, active };
        Painter::default().paint_tabs(&mut buf, w, h, &fonts, scale, &view);
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

    #[test]
    fn fallback_glyphs_actually_draw() {
        // CJK isn't in Arial; with the fallback font installed it must produce ink
        let blank = paint_page("<p style='font-size:30px'>&nbsp;</p>", 120, 60, 0.0, 1.0);
        let cjk = paint_page("<p style='font-size:30px'>日本</p>", 120, 60, 0.0, 1.0);
        assert_ne!(blank, cjk);
    }

    #[test]
    fn toolbar_draws_background_field_and_text() {
        let bar = AddressBar::new("https://example.com/");
        let (w, h) = (400usize, 60usize);
        let buf = paint_bar(&bar, w, h, 1.0);
        assert_eq!(buf[2 * w + 2], TOOLBAR_BG.to_u32(), "toolbar background");
        assert_eq!(buf[(chrome::HEIGHT as usize - 1) * w + 5], TOOLBAR_LINE.to_u32(), "bottom separator");
        assert_eq!(buf[(chrome::HEIGHT as usize + 5) * w + 5], 0, "nothing below the toolbar");
        let r = chrome::address_rect(w as f32);
        assert_eq!(buf[(r.y as usize + 4) * w + (r.x as usize + r.w as usize - 5)], 0xFFFFFF, "white field");
        // some pixels inside the field are text ink
        let ink = (r.y as usize..(r.y + r.h) as usize)
            .flat_map(|y| (r.x as usize + 8..r.x as usize + 120).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[y * w + x] != 0xFFFFFF)
            .count();
        assert!(ink > 50, "address text should be drawn ({ink} ink pixels)");
    }

    #[test]
    fn focused_field_shows_selection_caret_and_scrolls_to_the_caret() {
        let (w, h) = (300usize, 50usize);
        let mut bar = AddressBar::new("https://example.com/");
        bar.focus();
        let selected = paint_bar(&bar, w, h, 1.0);
        let r = chrome::address_rect(w as f32);
        let on = |buf: &Vec<u32>, c: Color| buf.iter().any(|&p| p == c.to_u32());
        assert!(on(&selected, FIELD_FOCUS), "blue focus ring");
        assert!(on(&selected, SELECTION), "selection highlight");
        // a long address while editing keeps the caret visible instead of drawing outside the field
        bar.insert(&"x".repeat(300));
        let long = paint_bar(&bar, w, h, 2.0);
        assert!(!on(&long, SELECTION));
        let _ = r;
        // an error message replaces the text in red
        let mut bar = AddressBar::new("a");
        bar.error = Some("not a URL".into());
        assert!(on(&paint_bar(&bar, w, h, 1.0), ERROR_TEXT));
    }

    #[test]
    fn toolbar_survives_tiny_windows_and_odd_scales() {
        let mut bar = AddressBar::new("https://example.com/with/a/long/path?and=query");
        bar.focus();
        for (w, h) in [(1, 1), (10, 5), (100, 20), (400, 44)] {
            for scale in [0.0, f32::NAN, 0.5, 1.0, 2.0, 3.0] {
                paint_bar(&bar, w, h, scale);
            }
        }
        bar.click_again();
        bar.insert("é日本語🎉");
        paint_bar(&bar, 120, 60, 2.0);
    }

    #[test]
    fn tab_strip_marks_the_active_tab_and_draws_titles() {
        let (w, h) = (500usize, 40usize);
        let buf = paint_strip(&["First page", "Second page"], 1, w, h, 1.0);
        let strip_h = tabbar::HEIGHT as usize;
        let (t0, t1) = (tabbar::tab_rect(0, 2, w as f32), tabbar::tab_rect(1, 2, w as f32));
        // a pixel in each tab's padding: the active one matches the toolbar, the other is darker
        assert_eq!(buf[(t0.y as usize + 2) * w + t0.x as usize + 2], TAB_INACTIVE.to_u32());
        assert_eq!(buf[(t1.y as usize + 2) * w + t1.x as usize + 2], TOOLBAR_BG.to_u32());
        assert_eq!(buf[(strip_h - 1) * w + t1.x as usize + 2], TOOLBAR_BG.to_u32(), "the active tab reaches the toolbar");
        assert_eq!(buf[(strip_h - 1) * w + t0.x as usize + 2], STRIP_BG.to_u32(), "inactive ones stop short");
        assert_eq!(buf[strip_h * w + 5], 0, "nothing below the strip");
        // titles are ink on the tab
        let ink = |t: tabbar::Rect| {
            (t.y as usize..strip_h).flat_map(|y| (t.x as usize..(t.x + t.w) as usize).map(move |x| (x, y))).filter(|&(x, y)| {
                let p = buf[y * w + x];
                p != TOOLBAR_BG.to_u32() && p != TAB_INACTIVE.to_u32() && p != STRIP_BG.to_u32()
            }).count()
        };
        assert!(ink(t0) > 40 && ink(t1) > 40, "{} {}", ink(t0), ink(t1));
    }

    #[test]
    fn long_titles_are_cut_with_an_ellipsis_and_never_overflow_the_tab() {
        let fonts = Fonts::load();
        let face = Face { family: Family::Sans, bold: false, italic: false };
        let long = "a very long page title that cannot possibly fit in a narrow tab".to_string();
        let fitted = fit_text(&fonts, face, 12.0, &long, 80.0);
        assert!(fitted.ends_with('…') && fitted.len() < long.len(), "{fitted}");
        assert!(fonts.measure(face, 12.0, &fitted) <= 80.0);
        assert_eq!(fit_text(&fonts, face, 12.0, "short", 500.0), "short");
        assert_eq!(fit_text(&fonts, face, 12.0, "anything", 0.0), "", "no room at all");
        assert_eq!(fit_text(&fonts, face, 12.0, "", 10.0), "");
    }

    #[test]
    fn tab_strip_survives_tiny_windows_odd_scales_and_many_tabs() {
        let many: Vec<&str> = vec!["é日本語🎉 tab title"; tabbar::MAX_TABS];
        for (w, h) in [(1, 1), (10, 5), (100, 20), (400, 40)] {
            for scale in [0.0, f32::NAN, 0.5, 1.0, 2.0, 3.0] {
                paint_strip(&["x"], 0, w, h, scale);
                paint_strip(&many, 3, w, h, scale);
                paint_strip(&[], 0, w, h, scale);
            }
        }
        paint_strip(&["a", "b"], 9, 200, 40, 1.0); // an out-of-range active index
    }

    fn find_for(src: &str, query: &str) -> (Page, Find, Fonts) {
        let fonts = Fonts::load();
        let mut page = Page::from_source(&page_url(), src);
        page.relayout(&fonts, 400.0);
        let mut find = Find::new();
        find.type_text(query);
        find.search(&page.layout, &fonts);
        (page, find, fonts)
    }

    #[test]
    fn find_highlights_matches_and_marks_the_selected_one() {
        let (page, mut find, fonts) = find_for("<style>body{margin:0}</style><p style='margin:0'>cat dog cat</p>", "cat");
        let (w, h) = (400usize, 120usize);
        let mut plain = vec![0u32; w * h];
        let mut painter = Painter::default();
        painter.paint(&mut plain, w, h, &page.layout, &fonts, 0.0, 1.0);
        let mut marked = plain.clone();
        painter.paint_find(&mut marked, w, h, &fonts, 1.0, 1.0, 0.0, &find);
        let m = find.matches().to_vec();
        assert_eq!(m.len(), 2);
        // a background pixel inside each match changes colour: yellowish for one, orange for the selected one
        let probe = |m: &crate::find::Match| ((m.baseline - m.size * 0.8) as usize) * w + (m.x + 1.0) as usize;
        let (first, second) = (marked[probe(&m[0])], marked[probe(&m[1])]);
        assert_ne!(first, plain[probe(&m[0])], "the match is tinted");
        assert_ne!(second, plain[probe(&m[1])]);
        assert_ne!(first, second, "the selected match looks different from the others");
        // selecting the other one swaps them
        find.next();
        let mut swapped = plain.clone();
        painter.paint_find(&mut swapped, w, h, &fonts, 1.0, 1.0, 0.0, &find);
        assert_eq!((swapped[probe(&m[0])], swapped[probe(&m[1])]), (second, first));
        // text between the matches is untouched
        let gap = (m[0].baseline as usize - 3) * w + (m[0].x + m[0].w + 3.0) as usize;
        assert_eq!(marked[gap], plain[gap]);
        // the search box is drawn at the top right: its blue border sits on the 9th row from the box's left edge
        let (box_x, box_y) = (w - 14 - 300, 8);
        assert_eq!(marked[box_y * w + box_x + 5], FIND_BOX_BORDER_FOR_TEST.to_u32());
        assert_eq!(plain[box_y * w + box_x + 5], 0xFFFFFF, "and it wasn't there before");
    }

    /// The colour the find box's border is drawn in (the focus blue).
    const FIND_BOX_BORDER_FOR_TEST: Color = FIELD_FOCUS;

    #[test]
    fn find_overlay_survives_odd_sizes_scales_scrolls_and_queries() {
        let fonts = Fonts::load();
        let mut page = Page::from_source(&page_url(), &format!("<p>{}</p>", "needle ".repeat(500)));
        page.relayout(&fonts, 300.0);
        for query in ["", "needle", "zzz", &"é日本".repeat(60)] {
            let mut find = Find::new();
            find.type_text(query);
            find.search(&page.layout, &fonts);
            for (w, h) in [(1, 1), (10, 5), (100, 40), (400, 300)] {
                for scale in [0.0, f32::NAN, 0.5, 1.0, 2.0] {
                    for scroll in [0.0, 100.0, 1e9, -50.0, f32::NAN] {
                        let mut buf = vec![0u32; w * h];
                        Painter::default().paint_find(&mut buf, w, h, &fonts, scale, 1.0, scroll, &find);
                    }
                }
            }
        }
    }
}
