//! Stage 4: styled DOM -> positioned boxes, emitted directly as a display list.
//!
//! Block layout: boxes stack vertically; width fills the container unless
//! `width`/`max-width` say otherwise; auto margins centre; adjacent vertical
//! margins collapse. Inline layout: text is flattened into words and
//! greedily wrapped into lines, then aligned.
use crate::css::{Color, Length};
use crate::dom::{Dom, Kind, NodeId};
use crate::fonts::{Face, Fonts};
use crate::images::Image;
use crate::style::{Align, Display, Style};
use std::collections::HashMap;
use std::rc::Rc;

pub enum Item {
    Nop,
    Rect { x: f32, y: f32, w: f32, h: f32, color: Color },
    /// `baseline` is the y of the text baseline.
    Text { x: f32, baseline: f32, text: String, size: f32, face: Face, color: Color },
    Image { x: f32, y: f32, w: f32, h: f32, img: Rc<Image> },
}

pub struct LinkRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub href: String,
}

pub struct Layout {
    pub items: Vec<Item>,
    pub links: Vec<LinkRegion>,
    pub height: f32,
    pub canvas_bg: Option<Color>,
}

pub fn layout_document(
    dom: &Dom,
    styles: &[Style],
    fonts: &Fonts,
    images: &HashMap<NodeId, Rc<Image>>,
    width: f32,
) -> Layout {
    let mut cx = Ctx { dom, styles, fonts, images, items: vec![], links: vec![] };
    let (bottom, mb) = cx.layout_block(0, 0.0, 0.0, 0.0, width, None, false);

    // The canvas takes the background of <html>, or else of <body>.
    let body = dom.nodes[0].children.iter().copied().find(|&c| dom.tag(c) == Some("body"));
    let canvas_bg = styles[0].background.or(body.and_then(|b| styles[b].background));
    Layout { items: cx.items, links: cx.links, height: bottom + mb, canvas_bg }
}

struct Ctx<'a> {
    dom: &'a Dom,
    styles: &'a [Style],
    fonts: &'a Fonts,
    images: &'a HashMap<NodeId, Rc<Image>>,
    items: Vec<Item>,
    links: Vec<LinkRegion>,
}

fn resolve(l: Length, base: f32) -> Option<f32> {
    match l {
        Length::Px(v) => Some(v),
        Length::Pct(p) => Some(base * p / 100.0),
        Length::Auto => None,
    }
}

/// CSS margin collapsing: two positives -> the larger; two negatives -> the
/// most negative; mixed signs -> their sum.
fn collapse(a: f32, b: f32) -> f32 {
    if a >= 0.0 && b >= 0.0 {
        a.max(b)
    } else if a < 0.0 && b < 0.0 {
        a.min(b)
    } else {
        a + b
    }
}

/// Flattened inline content.
enum Inl {
    Word { text: String, node: NodeId, pre: bool },
    Space(NodeId),
    Break(NodeId),
    /// A decoded image sitting on the baseline, already sized in CSS pixels.
    Image { node: NodeId, w: f32, h: f32 },
}

struct Placed {
    text: String,
    node: NodeId,
    x: f32,
    w: f32,
    href: Option<String>,
    /// Some(height) if this is an image rather than a word.
    image_h: Option<f32>,
}

struct Line {
    words: Vec<Placed>,
    asc: f32,
    desc: f32,
}

impl<'a> Ctx<'a> {
    /// Lay out a block-level box. `cursor` is the bottom border edge of the
    /// previous sibling (or the top of the container's content), `prev_mb` the
    /// previous sibling's bottom margin. Returns (bottom border edge, bottom
    /// margin to hand to the next sibling).
    ///
    /// `top_collapsed` means an ancestor already positioned us with our top
    /// margin collapsed into its own (parent/first-child collapsing).
    fn layout_block(
        &mut self,
        id: NodeId,
        left: f32,
        cursor: f32,
        prev_mb: f32,
        avail: f32,
        marker: Option<String>,
        top_collapsed: bool,
    ) -> (f32, f32) {
        let styles = self.styles;
        let s = &styles[id];

        let pad = s.padding.map(|l| resolve(l, avail).unwrap_or(0.0).max(0.0));
        let bw = s.border_w;
        let mb = resolve(s.margin[2], avail).unwrap_or(0.0);
        let (ml_opt, mr_opt) = (resolve(s.margin[3], avail), resolve(s.margin[1], avail));
        let extras = pad[1] + pad[3] + bw[1] + bw[3];
        // content-box width from a CSS width, honouring box-sizing
        let to_content = |w: f32| if s.border_box { (w - extras).max(0.0) } else { w.max(0.0) };

        // ---- horizontal: resolve content width and left/right margins
        let mut width_spec = resolve(s.width, avail).map(to_content);
        let inner = (avail - ml_opt.unwrap_or(0.0) - mr_opt.unwrap_or(0.0) - extras).max(0.0);
        let mut content_w = width_spec.unwrap_or(inner);
        if let Some(maxw) = s.max_width.map(to_content) {
            if content_w > maxw {
                content_w = maxw;
                width_spec = Some(maxw);
            }
        }
        let ml = if width_spec.is_some() {
            let leftover = (avail - content_w - extras).max(0.0);
            match (ml_opt, mr_opt) {
                (None, None) => leftover / 2.0,
                (None, Some(r)) => (leftover - r).max(0.0),
                (Some(l), _) => l,
            }
        } else {
            ml_opt.unwrap_or(0.0)
        };

        let bx = left + ml; // border-box left
        let border_w = content_w + extras;
        // Vertical margin collapsing: with no top padding/border, our top margin
        // merges with that of our first block child (recursively).
        let top_open = pad[0] + bw[0] == 0.0;
        let own_mt = if top_open { self.collapsed_top(id, avail) } else { resolve(s.margin[0], avail).unwrap_or(0.0) };
        let top = if top_collapsed { cursor } else { cursor + collapse(prev_mb, own_mt) };
        let content_x = bx + bw[3] + pad[3];
        let content_top = top + bw[0] + pad[0];

        let bg_slot = self.items.len();
        self.items.push(Item::Nop);

        if let Some(m) = marker {
            let face = s.face();
            let (asc, _) = self.fonts.vmetrics(face, s.font_size);
            let mw = self.fonts.measure(face, s.font_size, &m);
            self.items.push(Item::Text {
                x: content_x - mw - 6.0,
                baseline: content_top + asc,
                text: m,
                size: s.font_size,
                face,
                color: s.color,
            });
        }

        let (end_y, last_mb) = self.layout_children(id, content_x, content_top, content_w, top_open);

        // ---- vertical: the last child's margin collapses through if nothing separates it
        let mut content_bottom = end_y;
        let mut eff_mb = mb;
        if pad[2] + bw[2] == 0.0 && s.height.is_none() {
            eff_mb = collapse(mb, last_mb);
        } else {
            content_bottom += last_mb;
        }
        if let Some(h) = s.height {
            let h = if s.border_box { (h - pad[0] - pad[2] - bw[0] - bw[2]).max(0.0) } else { h.max(0.0) };
            content_bottom = content_top + h;
        }
        let bottom = content_bottom + pad[2] + bw[2];

        if let Some(color) = s.background {
            self.items[bg_slot] = Item::Rect { x: bx, y: top, w: border_w, h: bottom - top, color };
        }
        let bc = |i: usize| s.border_c[i].unwrap_or(s.color);
        if bw[0] > 0.0 {
            self.items.push(Item::Rect { x: bx, y: top, w: border_w, h: bw[0], color: bc(0) });
        }
        if bw[2] > 0.0 {
            self.items.push(Item::Rect { x: bx, y: bottom - bw[2], w: border_w, h: bw[2], color: bc(2) });
        }
        if bw[3] > 0.0 {
            self.items.push(Item::Rect { x: bx, y: top, w: bw[3], h: bottom - top, color: bc(3) });
        }
        if bw[1] > 0.0 {
            self.items.push(Item::Rect { x: bx + border_w - bw[1], y: top, w: bw[1], h: bottom - top, color: bc(1) });
        }
        (bottom, eff_mb)
    }

    /// Lay out the children of a block: block children stack; consecutive
    /// inline children form an anonymous inline formatting context.
    /// Returns (y after the last child's border box, its bottom margin).
    fn layout_children(&mut self, id: NodeId, x: f32, y: f32, width: f32, top_open: bool) -> (f32, f32) {
        let children = self.dom.nodes[id].children.clone();
        let (mut cursor, mut prev_mb) = (y, 0.0);
        let mut run: Vec<NodeId> = vec![];
        let mut counter = 0;
        let is_ol = self.dom.tag(id) == Some("ol");
        // The first block child whose top margin the parent has already absorbed.
        let collapsed_child = if top_open { self.first_block_child(id) } else { None };

        for c in children {
            match self.styles[c].display {
                Display::None => {}
                Display::Inline => run.push(c),
                d => {
                    self.flush_run(id, &mut run, x, &mut cursor, &mut prev_mb, width);
                    let mut marker = None;
                    if d == Display::ListItem {
                        counter += 1;
                        if !self.styles[id].list_none && !self.styles[c].list_none {
                            marker = Some(if is_ol { format!("{counter}.") } else { "•".to_string() });
                        }
                    }
                    let collapsed = collapsed_child == Some(c);
                    let (b, mb) = self.layout_block(c, x, cursor, prev_mb, width, marker, collapsed);
                    cursor = b;
                    prev_mb = mb;
                }
            }
        }
        self.flush_run(id, &mut run, x, &mut cursor, &mut prev_mb, width);
        (cursor, prev_mb)
    }

    /// The first child that produces content, if it is block-level (so its top
    /// margin can collapse with its parent's). Whitespace-only text and
    /// `display: none` nodes are skipped; any inline content first blocks collapsing.
    fn first_block_child(&self, id: NodeId) -> Option<NodeId> {
        for &c in &self.dom.nodes[id].children {
            let s = &self.styles[c];
            if s.display == Display::None {
                continue;
            }
            if let Kind::Text(t) = &self.dom.nodes[c].kind {
                if t.bytes().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
            }
            return matches!(s.display, Display::Block | Display::ListItem).then_some(c);
        }
        None
    }

    /// A block's top margin after collapsing with its first block child's.
    fn collapsed_top(&self, id: NodeId, avail: f32) -> f32 {
        let s = &self.styles[id];
        let mt = resolve(s.margin[0], avail).unwrap_or(0.0);
        let pad_top = resolve(s.padding[0], avail).unwrap_or(0.0);
        if pad_top + s.border_w[0] == 0.0 {
            if let Some(c) = self.first_block_child(id) {
                return collapse(mt, self.collapsed_top(c, avail));
            }
        }
        mt
    }

    fn flush_run(
        &mut self,
        parent: NodeId,
        run: &mut Vec<NodeId>,
        x: f32,
        cursor: &mut f32,
        prev_mb: &mut f32,
        width: f32,
    ) {
        if run.is_empty() {
            return;
        }
        let mut items = vec![];
        for &n in run.iter() {
            self.flatten(n, &mut items);
        }
        run.clear();
        if items.iter().all(|i| matches!(i, Inl::Space(_))) {
            return; // only whitespace between blocks: no anonymous box
        }
        *cursor += *prev_mb;
        *prev_mb = 0.0;
        let align = self.styles[parent].align;
        *cursor += self.layout_inline(&items, x, *cursor, width, align);
    }

    fn flatten(&self, id: NodeId, out: &mut Vec<Inl>) {
        let s = &self.styles[id];
        match &self.dom.nodes[id].kind {
            Kind::Text(t) => {
                if s.pre {
                    let t = t.strip_prefix('\n').unwrap_or(t);
                    for (i, line) in t.split('\n').enumerate() {
                        if i > 0 {
                            out.push(Inl::Break(id));
                        }
                        if !line.is_empty() {
                            out.push(Inl::Word { text: line.replace('\t', "    "), node: id, pre: true });
                        }
                    }
                    return;
                }
                let mut word = String::new();
                let mut in_space = false;
                for ch in t.chars() {
                    if ch.is_ascii_whitespace() {
                        if !word.is_empty() {
                            out.push(Inl::Word { text: std::mem::take(&mut word), node: id, pre: false });
                        }
                        in_space = true;
                    } else {
                        if in_space {
                            out.push(Inl::Space(id));
                            in_space = false;
                        }
                        word.push(ch);
                    }
                }
                if !word.is_empty() {
                    out.push(Inl::Word { text: word, node: id, pre: false });
                }
                if in_space {
                    out.push(Inl::Space(id));
                }
            }
            Kind::Element { tag, .. } => {
                if s.display == Display::None {
                    return;
                }
                match tag.as_str() {
                    "br" => out.push(Inl::Break(id)),
                    "img" => match self.images.get(&id) {
                        Some(img) => {
                            let (w, h) = self.image_size(id, img);
                            out.push(Inl::Image { node: id, w, h });
                        }
                        None => {
                            // Missing or undecodable image: fall back to its alt text.
                            let alt = self.dom.attr(id, "alt").unwrap_or("image");
                            out.push(Inl::Word { text: format!("[{alt}]"), node: id, pre: false });
                        }
                    },
                    _ => {
                        for &c in &self.dom.nodes[id].children {
                            self.flatten(c, out);
                        }
                    }
                }
            }
        }
    }

    /// Used size of an <img> in CSS px: CSS `width`/`height` win over the
    /// attributes; a single given dimension keeps the aspect ratio.
    fn image_size(&self, id: NodeId, img: &Image) -> (f32, f32) {
        let attr = |name: &str| {
            let v = self.dom.attr(id, name)?;
            let digits: String = v.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
            digits.parse::<f32>().ok().filter(|v| *v > 0.0)
        };
        let s = &self.styles[id];
        let w = match s.width { Length::Px(v) => Some(v), _ => attr("width") };
        let h = s.height.or_else(|| attr("height"));
        let (nw, nh) = (img.w as f32, img.h as f32);
        let (w, h) = match (w, h) {
            (Some(w), Some(h)) => (w, h),
            (Some(w), None) => (w, w * nh / nw),
            (None, Some(h)) => (h * nw / nh, h),
            (None, None) => (nw, nh),
        };
        // Keep sizes finite and sane so later arithmetic can't blow up.
        (w.clamp(0.0, 20_000.0), h.clamp(0.0, 20_000.0))
    }

    fn href_of(&self, mut id: NodeId) -> Option<String> {
        loop {
            if self.dom.tag(id) == Some("a") {
                if let Some(h) = self.dom.attr(id, "href") {
                    return Some(h.to_string());
                }
            }
            id = self.dom.nodes[id].parent?;
        }
    }

    /// Greedy line breaking. Returns the height consumed.
    fn layout_inline(&mut self, items: &[Inl], x: f32, y: f32, width: f32, align: Align) -> f32 {
        let (styles, fonts) = (self.styles, self.fonts);
        let new_line = || Line { words: vec![], asc: 0.0, desc: 0.0 };
        let mut lines = vec![new_line()];
        let mut cx = 0.0f32;
        let mut pending_space: Option<NodeId> = None;

        for it in items {
            // Everything placeable (a word or an image) reduces to the same tuple.
            let (text, node, w, a, d, pre, image_h) = match it {
                Inl::Space(n) => {
                    pending_space = Some(*n);
                    continue;
                }
                Inl::Break(n) => {
                    let s = &styles[*n];
                    let (a, d) = fonts.vmetrics(s.face(), s.font_size);
                    let line = lines.last_mut().unwrap();
                    line.asc = line.asc.max(a);
                    line.desc = line.desc.max(d);
                    lines.push(new_line());
                    cx = 0.0;
                    pending_space = None;
                    continue;
                }
                Inl::Word { text, node, pre } => {
                    let s = &styles[*node];
                    let (a, d) = fonts.vmetrics(s.face(), s.font_size);
                    (text.clone(), *node, fonts.measure(s.face(), s.font_size, text), a, d, *pre, None)
                }
                Inl::Image { node, w, h } => (String::new(), *node, *w, *h, 0.0, false, Some(*h)),
            };
            let mut sp = match pending_space {
                Some(sn) if !lines.last().unwrap().words.is_empty() => {
                    let ss = &styles[sn];
                    fonts.measure(ss.face(), ss.font_size, " ")
                }
                _ => 0.0,
            };
            if !pre && !lines.last().unwrap().words.is_empty() && cx + sp + w > width {
                lines.push(new_line());
                cx = 0.0;
                sp = 0.0;
            }
            let href = self.href_of(node);
            let line = lines.last_mut().unwrap();
            line.asc = line.asc.max(a);
            line.desc = line.desc.max(d);
            line.words.push(Placed { text, node, x: cx + sp, w, href, image_h });
            cx += sp + w;
            pending_space = None;
        }

        // ---- emit
        let mut cy = y;
        for line in &lines {
            let line_h = line.asc + line.desc;
            if line_h == 0.0 {
                continue;
            }
            let line_w = line.words.last().map_or(0.0, |w| w.x + w.w);
            let off = match align {
                Align::Left => 0.0,
                Align::Center => ((width - line_w) / 2.0).max(0.0),
                Align::Right => (width - line_w).max(0.0),
            };
            let baseline = cy + line.asc;
            for (i, w) in line.words.iter().enumerate() {
                let s = &styles[w.node];
                let face = s.face();
                let ax = x + off + w.x;
                if let (Some(h), Some(img)) = (w.image_h, self.images.get(&w.node)) {
                    self.items.push(Item::Image { x: ax, y: baseline - h, w: w.w, h, img: img.clone() });
                    if let Some(href) = &w.href {
                        self.links.push(LinkRegion { x: ax, y: baseline - h, w: w.w, h, href: href.clone() });
                    }
                    continue;
                }
                // Decorations extend across the space to the next word if it matches.
                let joined = line.words.get(i + 1).filter(|n| {
                    n.href == w.href && styles[n.node].underline == s.underline
                        && self.dom.nodes[n.node].parent == self.dom.nodes[w.node].parent
                });
                let span = joined.map_or(w.w, |n| n.x - w.x);

                let parent_bg = self.dom.nodes[w.node].parent.and_then(|p| styles[p].background);
                if let Some(color) = parent_bg {
                    let (a, d) = fonts.vmetrics(face, s.font_size);
                    self.items.push(Item::Rect { x: ax, y: baseline - a, w: span, h: a + d, color });
                }
                self.items.push(Item::Text {
                    x: ax, baseline, text: w.text.clone(), size: s.font_size, face, color: s.color,
                });
                if s.underline {
                    let t = (s.font_size / 16.0).max(1.0);
                    self.items.push(Item::Rect {
                        x: ax, y: baseline + s.font_size * 0.12, w: span, h: t, color: s.color,
                    });
                }
                if let Some(href) = &w.href {
                    self.links.push(LinkRegion { x: ax, y: cy, w: span, h: line_h, href: href.clone() });
                }
            }
            cy += line_h;
        }
        cy - y
    }
}
