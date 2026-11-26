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
    // Which inline elements contain block-level descendants? Children come
    // after their parents in the arena, so one backwards pass is enough.
    let mut has_block = vec![false; dom.nodes.len()];
    for id in (0..dom.nodes.len()).rev() {
        has_block[id] = dom.nodes[id].children.iter().any(|&c| match styles[c].display {
            Display::Block | Display::ListItem => true,
            Display::Inline => has_block[c],
            Display::None => false,
        });
    }
    let mut cx = Ctx { dom, styles, fonts, images, has_block, items: vec![], links: vec![] };
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
    /// `has_block[i]`: element `i` is inline but contains block-level boxes.
    has_block: Vec<bool>,
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
    /// Has a word, image or forced break: empty trailing lines are not drawn.
    content: bool,
}

impl<'a> Ctx<'a> {
    /// Lay out a block-level box. `cursor` is the bottom border edge of the
    /// previous sibling (or the top of the container's content), `prev_mb` the
    /// previous sibling's bottom margin. Returns (bottom border edge, bottom
    /// margin to hand to the next sibling).
    ///
    /// `top_collapsed` means an ancestor already positioned us with our top
    /// margin collapsed into its own (parent/first-child collapsing).
    #[allow(clippy::too_many_arguments)]
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
        let bw = s.borders();
        let mt_raw = resolve(s.margin[0], avail).unwrap_or(0.0);
        let mb = resolve(s.margin[2], avail).unwrap_or(0.0);

        // An empty block (no content, no height, padding or border) has no box
        // to speak of: its top and bottom margins collapse with each other and
        // with its neighbours' ("collapse through").
        if marker.is_none()
            && pad[0] + pad[2] + bw[0] + bw[2] == 0.0
            && s.height.is_none_or(|h| h <= 0.0)
            && self.is_empty_block(id)
        {
            let merged = if top_collapsed { mb } else { collapse(prev_mb, collapse(mt_raw, mb)) };
            return (cursor, merged);
        }

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
        let own_mt = if top_open { self.collapsed_top(id, avail) } else { mt_raw };
        let top = if top_collapsed { cursor } else { cursor + collapse(prev_mb, own_mt) };
        let content_x = bx + bw[3] + pad[3];
        let content_top = top + bw[0] + pad[0];

        let bg_slot = self.items.len();
        self.items.push(Item::Nop);

        let marker_slot = marker.map(|m| {
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
            self.items.len() - 1
        });

        let (end_y, last_mb) = self.layout_children(id, content_x, content_top, content_w, top_open);

        // A list marker sits on the baseline of the item's first line, whatever
        // font that line uses (a heading inside an <li> has a taller baseline).
        if let Some(ms) = marker_slot {
            let first = self.items[ms + 1..].iter().find_map(|i| match i {
                Item::Text { baseline, .. } => Some(*baseline),
                Item::Image { y, h, .. } => Some(y + h),
                _ => None,
            });
            if let (Some(b), Item::Text { baseline, .. }) = (first, &mut self.items[ms]) {
                *baseline = b;
            }
        }

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
        // None = transparent border: it still takes space but draws nothing
        let bc = |i: usize| match s.border_c[i] {
            None => Some(s.color),
            Some(c) => c,
        };
        if let (true, Some(color)) = (bw[0] > 0.0, bc(0)) {
            self.items.push(Item::Rect { x: bx, y: top, w: border_w, h: bw[0], color });
        }
        if let (true, Some(color)) = (bw[2] > 0.0, bc(2)) {
            self.items.push(Item::Rect { x: bx, y: bottom - bw[2], w: border_w, h: bw[2], color });
        }
        if let (true, Some(color)) = (bw[3] > 0.0, bc(3)) {
            self.items.push(Item::Rect { x: bx, y: top, w: bw[3], h: bottom - top, color });
        }
        if let (true, Some(color)) = (bw[1] > 0.0, bc(1)) {
            self.items.push(Item::Rect { x: bx + border_w - bw[1], y: top, w: bw[1], h: bottom - top, color });
        }
        (bottom, eff_mb)
    }

    /// The children that take part in `id`'s formatting context. An inline
    /// element that contains block boxes (`<a><div>..</div></a>`) is looked
    /// through: its children join the parent's flow, as if it had no box of its own.
    fn flow_children(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = vec![];
        self.collect_flow(id, &mut out);
        out
    }

    fn collect_flow(&self, id: NodeId, out: &mut Vec<NodeId>) {
        for &c in &self.dom.nodes[id].children {
            if self.styles[c].display == Display::Inline && self.has_block[c] {
                self.collect_flow(c, out);
            } else {
                out.push(c);
            }
        }
    }

    /// Would this inline-level child draw nothing at all (only whitespace)?
    fn inline_is_blank(&self, c: NodeId) -> bool {
        let mut items = vec![];
        self.flatten(c, &mut items);
        items.iter().all(|i| matches!(i, Inl::Space(_)))
    }

    /// No visible content in any child (hidden nodes, whitespace, empty inlines).
    fn is_empty_block(&self, id: NodeId) -> bool {
        self.flow_children(id).iter().all(|&c| match self.styles[c].display {
            Display::None => true,
            Display::Inline => self.inline_is_blank(c),
            _ => false,
        })
    }

    /// Lay out the children of a block: block children stack; consecutive
    /// inline children form an anonymous inline formatting context.
    /// Returns (y after the last child's border box, its bottom margin).
    fn layout_children(&mut self, id: NodeId, x: f32, y: f32, width: f32, top_open: bool) -> (f32, f32) {
        let children = self.flow_children(id);
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
    /// margin can collapse with its parent's). Whitespace-only text, empty
    /// inlines and `display: none` nodes are skipped; any real inline content
    /// first blocks collapsing.
    fn first_block_child(&self, id: NodeId) -> Option<NodeId> {
        for c in self.flow_children(id) {
            match self.styles[c].display {
                Display::None => continue,
                Display::Inline => {
                    if self.inline_is_blank(c) {
                        continue;
                    }
                    return None;
                }
                _ => return Some(c),
            }
        }
        None
    }

    /// A block's top margin after collapsing with its first block child's.
    fn collapsed_top(&self, id: NodeId, avail: f32) -> f32 {
        let s = &self.styles[id];
        let mt = resolve(s.margin[0], avail).unwrap_or(0.0);
        let pad_top = resolve(s.padding[0], avail).unwrap_or(0.0);
        if pad_top + s.borders()[0] == 0.0 {
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
        *cursor += self.layout_inline(&items, x, *cursor, width, align, parent);
    }

    fn flatten(&self, id: NodeId, out: &mut Vec<Inl>) {
        let s = &self.styles[id];
        match &self.dom.nodes[id].kind {
            Kind::Text(t) => {
                if s.pre {
                    // Only the newline right after <pre> is dropped; later text
                    // nodes (e.g. after a highlighted <span>) keep theirs.
                    let parent = self.dom.nodes[id].parent;
                    let first_in_pre = parent.is_some_and(|p| {
                        self.dom.tag(p) == Some("pre") && self.dom.nodes[p].children.first() == Some(&id)
                    });
                    let t = if first_in_pre { t.strip_prefix('\n').unwrap_or(t) } else { t };
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
                        // An explicit empty alt means "decorative": show nothing.
                        None => match self.dom.attr(id, "alt") {
                            Some("") => {}
                            alt => {
                                let alt = alt.unwrap_or("image");
                                out.push(Inl::Word { text: format!("[{alt}]"), node: id, pre: false });
                            }
                        },
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

    /// Is `node` equal to, or inside, `ancestor`?
    fn is_within(&self, mut node: NodeId, ancestor: NodeId) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            match self.dom.nodes[node].parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }

    /// Backgrounds of the inline elements around `node`, outermost first.
    /// (A block parent's background is painted by the block itself.)
    fn inline_backgrounds(&self, node: NodeId) -> Vec<(NodeId, Color)> {
        let mut out = vec![];
        let mut cur = self.dom.nodes[node].parent;
        while let Some(p) = cur {
            let s = &self.styles[p];
            if s.display != Display::Inline {
                break;
            }
            if let Some(c) = s.background {
                out.push((p, c));
            }
            cur = self.dom.nodes[p].parent;
        }
        out.reverse();
        out
    }

    /// Greedy line breaking. Returns the height consumed. `parent` is the block
    /// whose font gives every line its minimum height (the "strut").
    fn layout_inline(&mut self, items: &[Inl], x: f32, y: f32, width: f32, align: Align, parent: NodeId) -> f32 {
        let (styles, fonts) = (self.styles, self.fonts);
        let ps = &styles[parent];
        let (strut_a, strut_d) = fonts.vmetrics(ps.face(), ps.font_size);
        let new_line = || Line { words: vec![], asc: strut_a, desc: strut_d, content: false };
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
                    line.content = true;
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
            line.content = true;
            line.words.push(Placed { text, node, x: cx + sp, w, href, image_h });
            cx += sp + w;
            pending_space = None;
        }

        // ---- emit
        let mut cy = y;
        for line in &lines {
            if !line.content {
                continue;
            }
            let line_h = line.asc + line.desc;
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
                let next = line.words.get(i + 1);
                if let (Some(h), Some(img)) = (w.image_h, self.images.get(&w.node)) {
                    self.items.push(Item::Image { x: ax, y: baseline - h, w: w.w, h, img: img.clone() });
                    if let Some(href) = &w.href {
                        self.links.push(LinkRegion { x: ax, y: baseline - h, w: w.w, h, href: href.clone() });
                    }
                    continue;
                }
                // A link or underline continues across the space to the next word
                // when that word is part of the same link and decorated alike.
                let joined = next.filter(|n| n.href == w.href && styles[n.node].underline == s.underline);
                let span = joined.map_or(w.w, |n| n.x - w.x);

                // One background rect per enclosing inline element, extended over the
                // space to the next word if that word is inside the same element.
                for (anc, color) in self.inline_backgrounds(w.node) {
                    let (a, d) = fonts.vmetrics(face, s.font_size);
                    let bg_span = match next {
                        Some(n) if self.is_within(n.node, anc) => n.x - w.x,
                        _ => w.w,
                    };
                    self.items.push(Item::Rect { x: ax, y: baseline - a, w: bg_span, h: a + d, color });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use url::Url;

    fn render(body: &str) -> Page {
        let fonts = Fonts::load();
        let src = format!("<style>body{{margin:0}}</style>{body}");
        let mut p = Page::from_source(&Url::parse("file:///tmp/t.html").unwrap(), &src);
        p.relayout(&fonts, 400.0);
        p
    }

    fn texts(p: &Page) -> Vec<(String, f32)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Text { text, baseline, .. } => Some((text.clone(), *baseline)),
            _ => None,
        }).collect()
    }

    fn rects(p: &Page) -> Vec<(f32, f32, f32, f32, Color)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Rect { x, y, w, h, color } => Some((*x, *y, *w, *h, *color)),
            _ => None,
        }).collect()
    }

    #[test]
    fn blocks_inside_inline_elements_keep_their_boxes() {
        // A "card" link: <a> wrapping block divs used to be flattened into one text run.
        let p = render("<a href=x><div style='background:#fcc;padding:10px;margin:20px'>Card title</div><div>second block</div></a>");
        let t = texts(&p);
        let (card, second) = (t.iter().find(|(s, _)| s == "Card").unwrap(), t.iter().find(|(s, _)| s == "second").unwrap());
        assert!(second.1 > card.1 + 10.0, "blocks stack vertically: {t:?}");
        let r = rects(&p).into_iter().find(|r| r.4 == Color { r: 255, g: 204, b: 204 }).expect("card background");
        assert!(r.3 > 30.0, "padding applies: height {}", r.3);
        assert_eq!(r.0, 20.0, "margin applies");
        assert!(p.layout.links.len() >= 3, "text in both blocks is still a link");
    }

    #[test]
    fn pre_strips_only_the_newline_right_after_the_tag() {
        let p = render("<pre style='margin:0'><span>a</span>\n<span>b</span>\n</pre>");
        let t = texts(&p);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[1].1 > t[0].1 + 5.0, "a and b are on separate lines: {t:?}");
        let p = render("<pre style='margin:0'>\nfirst\nsecond</pre>");
        let t = texts(&p);
        assert_eq!(t[0].0, "first");
        assert!(t[1].1 > t[0].1);
    }

    #[test]
    fn empty_blocks_collapse_their_margins_through() {
        let p = render(
            "<div style='height:20px;margin-bottom:20px;background:#ddd'></div>\
             <div style='margin:20px 0'></div>\
             <div style='height:20px;margin-top:20px;background:red'></div>",
        );
        let red = rects(&p).into_iter().find(|r| r.4 == Color { r: 255, g: 0, b: 0 }).unwrap();
        // 20 (grey) + one collapsed 20px gap = 40; it used to be 60
        assert_eq!(red.1, 40.0);
        // an empty <p> between paragraphs adds no extra space beyond one margin
        let with = render("<p style='margin:10px 0'>a</p><p style='margin:10px 0'></p><p style='margin:10px 0'>b</p>");
        let without = render("<p style='margin:10px 0'>a</p><p style='margin:10px 0'>b</p>");
        assert_eq!(with.layout.height, without.layout.height);
        // but padding or a border means it is not empty
        let boxed = render("<div style='padding:5px'></div>");
        assert_eq!(boxed.layout.height, 10.0);
    }

    #[test]
    fn lines_are_at_least_as_tall_as_the_blocks_own_font() {
        let p = render("<p style='margin:0;font-size:40px;background:#fcc'><small style='font-size:10px'>tiny</small></p>");
        let bg = rects(&p).into_iter().find(|r| r.4 == Color { r: 255, g: 204, b: 204 }).unwrap();
        assert!(bg.3 > 40.0, "line height should follow the 40px strut, got {}", bg.3);
        // a trailing <br> doesn't create a phantom extra line
        let one = render("<p style='margin:0'>line</p>");
        let br = render("<p style='margin:0'>line<br></p>");
        assert_eq!(one.layout.height, br.layout.height);
        // but two <br>s in a row do add a blank line
        let two = render("<p style='margin:0'>line<br><br>next</p>");
        assert!(two.layout.height > one.layout.height * 2.5);
    }

    #[test]
    fn inline_backgrounds_cover_nested_elements_and_the_gaps_between_words() {
        let p = render("<span style='background:#ffff00'>aa <b>bold</b> cc</span>");
        let mut y: Vec<_> = rects(&p).into_iter().filter(|r| r.4 == Color { r: 255, g: 255, b: 0 }).collect();
        y.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        assert_eq!(y.len(), 3, "one rect per word: {y:?}");
        for pair in y.windows(2) {
            assert!((pair[0].0 + pair[0].2 - pair[1].0).abs() < 0.01, "no gap between {pair:?}");
        }
        // a block's own background is painted once, not once per word
        let p = render("<p style='background:red;margin:0'>one two three</p>");
        assert_eq!(rects(&p).len(), 1);
    }

    #[test]
    fn list_marker_aligns_with_the_first_line_of_its_item() {
        let p = render("<ul style='margin:0'><li><h2 style='margin:0'>Big title</h2></li></ul>");
        let t = texts(&p);
        let bullet = t.iter().find(|(s, _)| s == "•").unwrap();
        let word = t.iter().find(|(s, _)| s == "Big").unwrap();
        assert_eq!(bullet.1, word.1);
        // plain items still work, and ordered lists number
        let p = render("<ol style='margin:0'><li>x</li><li>y</li></ol>");
        let t = texts(&p);
        assert!(t.iter().any(|(s, _)| s == "1.") && t.iter().any(|(s, _)| s == "2."));
    }

    #[test]
    fn links_and_underlines_continue_across_element_boundaries() {
        let p = render("<a href=x>Hello <em>world</em></a>");
        assert_eq!(p.layout.links.len(), 2);
        let (a, b) = (&p.layout.links[0], &p.layout.links[1]);
        assert!((a.x + a.w - b.x).abs() < 0.01, "no dead zone between the words: {} vs {}", a.x + a.w, b.x);
        // two different links next to each other must NOT be joined
        let p = render("<a href=x>one</a> <a href=y>two</a>");
        let (a, b) = (&p.layout.links[0], &p.layout.links[1]);
        assert!(a.x + a.w < b.x - 1.0);
    }

    #[test]
    fn empty_alt_means_decorative() {
        assert!(texts(&render("<img src=nope.png alt=''>")).is_empty());
        assert_eq!(texts(&render("<img src=nope.png alt='logo'>"))[0].0, "[logo]");
        assert_eq!(texts(&render("<img src=nope.png>"))[0].0, "[image]");
    }

    #[test]
    fn deeply_nested_inline_and_block_structures_do_not_blow_up() {
        let n = 400;
        let body = format!("{}text{}", "<span><div>".repeat(n), "</div></span>".repeat(n));
        let p = render(&body);
        assert!(texts(&p).iter().any(|(s, _)| s == "text"));
    }
}
