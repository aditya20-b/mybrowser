//! Ties the pipeline together for one document.
use crate::css::{self, Rule};
use crate::dom::{Dom, Kind};
use crate::fonts::Fonts;
use crate::layout::{self, Layout};
use crate::style::{self, Style};
use crate::html;
use std::path::{Path, PathBuf};

pub struct Page {
    pub path: PathBuf,
    pub title: String,
    pub dom: Dom,
    pub styles: Vec<Style>,
    pub layout: Layout,
}

impl Page {
    pub fn load(path: &Path) -> Page {
        // Lossy decoding: a stray invalid byte shouldn't make the whole page unreadable.
        match std::fs::read(path) {
            Ok(bytes) => Page::from_source(path, &String::from_utf8_lossy(&bytes)),
            Err(e) => Page::from_source(
                path,
                &format!(
                    "<title>Error</title><h1>Could not open page</h1><p><code>{}</code></p><p>{e}</p>",
                    path.display()
                ),
            ),
        }
    }

    pub fn from_source(path: &Path, src: &str) -> Page {
        // Normalise Windows/old-Mac line endings so <pre> doesn't render stray '\r' glyphs.
        let src = src.replace("\r\n", "\n").replace('\r', "\n");
        let dom = html::parse(&src);
        let dir = path.parent().unwrap_or(Path::new("."));

        // Stylesheets in cascade order: user agent, then document order.
        let mut rules: Vec<Rule> = vec![];
        css::parse_stylesheet(style::USER_AGENT_CSS, &mut rules);
        let mut title = String::new();
        for id in 0..dom.nodes.len() {
            let Kind::Element { tag, .. } = &dom.nodes[id].kind else { continue };
            match tag.as_str() {
                "style" => css::parse_stylesheet(&dom.text_content(id), &mut rules),
                "link" if dom.attr(id, "rel").is_some_and(|r| r.eq_ignore_ascii_case("stylesheet")) => {
                    if let Some(href) = dom.attr(id, "href") {
                        if let Ok(text) = std::fs::read_to_string(dir.join(href)) {
                            css::parse_stylesheet(&text, &mut rules);
                        }
                    }
                }
                "title" if title.is_empty() => title = dom.text_content(id).trim().to_string(),
                _ => {}
            }
        }
        let styles = style::compute(&dom, &rules);
        let layout = Layout { items: vec![], links: vec![], height: 0.0, canvas_bg: None };
        Page { path: path.to_path_buf(), title, dom, styles, layout }
    }

    /// Re-run layout for a new viewport width (logical pixels).
    pub fn relayout(&mut self, fonts: &Fonts, width: f32) {
        self.layout = layout::layout_document(&self.dom, &self.styles, fonts, width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Item;

    fn render(src: &str, width: f32) -> Page {
        let fonts = Fonts::load();
        let mut p = Page::from_source(Path::new("t.html"), src);
        p.relayout(&fonts, width);
        p
    }

    fn rects(p: &Page) -> Vec<(f32, f32, f32, f32)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Rect { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
            _ => None,
        }).collect()
    }

    #[test]
    fn sibling_margins_collapse() {
        // 10 + 20 + max(10,10) + 20 + 10 = 70 (not 90)
        let p = render("<style>body{margin:0}</style><div style='height:20px;margin:10px 0'></div><div style='height:20px;margin:10px 0'></div>", 400.0);
        assert_eq!(p.layout.height, 70.0);
    }

    #[test]
    fn negative_margins_collapse_by_summing_mixed_signs() {
        let p = render("<style>body{margin:0}</style><div style='height:20px;margin-bottom:30px'></div><div style='height:20px;margin-top:-10px'></div>", 400.0);
        // 20 + (30 + -10) + 20 = 60
        assert_eq!(p.layout.height, 60.0);
    }

    #[test]
    fn parent_and_first_child_top_margins_collapse() {
        let p = render("<style>body{margin:0}</style><div style='margin-top:8px'><div style='margin-top:20px;height:10px;background:red'></div></div>", 400.0);
        let red = rects(&p)[0];
        assert_eq!(red.1, 20.0, "child should start at max(8,20), not 8+20");
    }

    #[test]
    fn padding_or_border_stops_margin_collapsing() {
        let p = render("<style>body{margin:0}</style><div style='margin-top:8px;padding-top:1px'><div style='margin-top:20px;height:10px;background:red'></div></div>", 400.0);
        assert_eq!(rects(&p)[0].1, 8.0 + 1.0 + 20.0);
    }

    #[test]
    fn border_box_width_includes_padding() {
        let p = render("<style>body{margin:0}</style><div style='width:200px;padding:10px;box-sizing:border-box;background:red'>x</div>", 400.0);
        assert_eq!(rects(&p)[0].2, 200.0);
        let p = render("<style>body{margin:0}</style><div style='width:200px;padding:10px;background:red'>x</div>", 400.0);
        assert_eq!(rects(&p)[0].2, 220.0);
    }

    #[test]
    fn max_width_with_auto_margins_centres() {
        let p = render("<style>body{margin:0}</style><div style='max-width:100px;margin:0 auto;background:red'>x</div>", 400.0);
        let r = rects(&p)[0];
        assert_eq!((r.0, r.2), (150.0, 100.0));
    }

    #[test]
    fn text_wraps_in_narrow_viewport() {
        let src = "<p>the quick brown fox jumps over the lazy dog</p>";
        let lines = |w| {
            let p = render(src, w);
            let mut ys: Vec<i32> = p.layout.items.iter().filter_map(|i| match i {
                Item::Text { baseline, .. } => Some(*baseline as i32), _ => None }).collect();
            ys.dedup();
            ys.len()
        };
        assert_eq!(lines(2000.0), 1);
        let (mid, narrow) = (lines(120.0), lines(60.0));
        assert!(mid >= 3 && narrow > mid, "{mid} {narrow}");
    }

    #[test]
    fn negative_and_huge_sizes_do_not_panic_through_paint() {
        use crate::paint::Painter;
        let fonts = Fonts::load();
        let src = "<div style='width:-50px;height:-20px;padding:-5px;margin:-9999px;border:2px solid red;background:red'>x</div>\
                   <p style='font-size:99999999px'>y</p><div style='width:1px'>unbreakable_word_much_wider_than_container</div>";
        let mut p = Page::from_source(Path::new("t.html"), src);
        p.relayout(&fonts, 300.0);
        let (w, h) = (300usize, 200usize);
        Painter::default().paint(&mut vec![0; w * h], w, h, &p.layout, &fonts, 0.0, 2.0 / 2.0);
    }

    #[test]
    fn display_none_and_whitespace_between_blocks_add_no_height() {
        let p = render("<style>body{margin:0}</style><div style='height:10px'></div>\n  \n<span style='display:none'>hidden</span>\n<div style='height:10px'></div>", 400.0);
        assert_eq!(p.layout.height, 20.0);
    }

    #[test]
    fn pre_preserves_lines_and_spaces() {
        let p = render("<style>body{margin:0}pre{margin:0}</style><pre>a\n  b\n\nc</pre>", 400.0);
        let n = p.layout.items.iter().filter(|i| matches!(i, Item::Text { .. })).count();
        assert_eq!(n, 3); // "a", "  b", "c"; the blank line is just vertical space
    }
}
