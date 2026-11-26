//! Stage 4: styled DOM -> positioned boxes, emitted directly as a display list.
//!
//! Block layout: boxes stack vertically; width fills the container unless
//! `width`/`max-width` say otherwise; auto margins centre; adjacent vertical
//! margins collapse. Inline layout: text is flattened into words and
//! greedily wrapped into lines, then aligned. Tables use the classic
//! auto-layout idea: measure each column's min/max content width, fit the
//! table to the available space, then lay the cells out row by row. Form
//! controls are inline boxes drawn from rects and text.
use crate::css::{Color, Length};
use crate::dom::{Dom, Kind, NodeId};
use crate::fonts::{Face, Family, Fonts};
use crate::forms::{self, ControlKind, FormState};
use crate::images::Image;
use crate::style::{Align, Display, Style, VAlign};
use std::collections::HashMap;
use std::rc::Rc;

/// A table never has more columns than this (cells beyond it are dropped), so
/// `colspan=999999999` can't allocate gigabytes.
const MAX_COLS: usize = 1000;

/// Form controls use a fixed 13.333px sans-serif, like a browser's default.
const CTL_FONT: f32 = 13.333;
const CTL_BORDER: Color = Color { r: 118, g: 118, b: 118 };
const CTL_FOCUS: Color = Color { r: 26, g: 115, b: 232 };
const CTL_FACE: Color = Color { r: 239, g: 239, b: 239 };
const CTL_TEXT: Color = Color { r: 32, g: 32, b: 32 };
const CTL_PLACEHOLDER: Color = Color { r: 117, g: 117, b: 117 };
const CTL_DISABLED: Color = Color { r: 150, g: 150, b: 150 };

fn ctl_face() -> Face {
    Face { family: Family::Sans, bold: false, italic: false }
}

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

/// The area a form control occupies, for hit-testing clicks.
pub struct ControlRegion {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub node: NodeId,
}

pub struct Layout {
    pub items: Vec<Item>,
    pub links: Vec<LinkRegion>,
    pub controls: Vec<ControlRegion>,
    pub height: f32,
    pub canvas_bg: Option<Color>,
}

pub fn layout_document(
    dom: &Dom,
    styles: &[Style],
    fonts: &Fonts,
    images: &HashMap<NodeId, Rc<Image>>,
    form: &FormState,
    width: f32,
) -> Layout {
    // Which inline elements contain block-level descendants? Children come
    // after their parents in the arena, so one backwards pass is enough.
    let mut has_block = vec![false; dom.nodes.len()];
    for id in (0..dom.nodes.len()).rev() {
        has_block[id] = dom.nodes[id].children.iter().any(|&c| match styles[c].display {
            Display::Inline => has_block[c],
            Display::None => false,
            _ => true,
        });
    }
    let mut cx = Ctx { dom, styles, fonts, images, form, has_block, items: vec![], links: vec![], controls: vec![] };
    let (bottom, mb) = cx.layout_block(0, 0.0, 0.0, 0.0, width, None, false);

    // The canvas takes the background of <html>, or else of <body>.
    let body = dom.nodes[0].children.iter().copied().find(|&c| dom.tag(c) == Some("body"));
    let canvas_bg = styles[0].background.or(body.and_then(|b| styles[b].background));
    Layout { items: cx.items, links: cx.links, controls: cx.controls, height: bottom + mb, canvas_bg }
}

struct Ctx<'a> {
    dom: &'a Dom,
    styles: &'a [Style],
    fonts: &'a Fonts,
    images: &'a HashMap<NodeId, Rc<Image>>,
    form: &'a FormState,
    /// `has_block[i]`: element `i` is inline but contains block-level boxes.
    has_block: Vec<bool>,
    items: Vec<Item>,
    links: Vec<LinkRegion>,
    controls: Vec<ControlRegion>,
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
    /// A form control: `asc` above the text baseline, `desc` below it.
    Control { node: NodeId, w: f32, asc: f32, desc: f32 },
}

struct Placed {
    text: String,
    node: NodeId,
    x: f32,
    w: f32,
    href: Option<String>,
    /// Some(height) if this is an image rather than a word.
    image_h: Option<f32>,
    /// Some((ascent, descent)) if this is a form control.
    control: Option<(f32, f32)>,
}

struct Line {
    words: Vec<Placed>,
    asc: f32,
    desc: f32,
    /// Has a word, image or forced break: empty trailing lines are not drawn.
    content: bool,
}

/// Where a box's background and border rects live in the display list, so a
/// table row can stretch them after the row's height is known.
struct Decor {
    bg_slot: usize,
    top: f32,
    bw: [f32; 4],
    border_slots: [Option<usize>; 4],
}

struct CellOut {
    height: f32,
    decor: Decor,
    /// Range of display items / link regions / controls that make up the cell's content.
    content: (usize, usize),
    links: (usize, usize),
    controls: (usize, usize),
}

struct Cell {
    id: NodeId,
    col: usize,
    span: usize,
    rowspan: usize,
}

struct RowInfo {
    id: NodeId,
    group: Option<NodeId>,
    cells: Vec<Cell>,
}

struct TableModel {
    rows: Vec<RowInfo>,
    captions: Vec<NodeId>,
    min: Vec<f32>,
    max: Vec<f32>,
    pct: Vec<Option<f32>>,
    spacing: f32,
    /// Outer border widths of the table box (CSS or the legacy `border` attribute).
    bw: [f32; 4],
    html_border: bool,
}

/// A cell that spans several rows, waiting for its last row to be measured.
struct Spanning {
    out: CellOut,
    first: usize,
    last: usize,
    valign: VAlign,
}

/// Divide `avail` between columns: percentage columns get their share, the rest
/// share what's left between their min and max content widths.
fn distribute(min: &[f32], max: &[f32], pct: &[Option<f32>], avail: f32) -> Vec<f32> {
    let n = min.len();
    let mut w = vec![0.0f32; n];
    let mut fixed = vec![false; n];
    for i in 0..n {
        if let Some(p) = pct[i] {
            w[i] = (avail * p / 100.0).max(min[i]);
            fixed[i] = true;
        }
    }
    let idx: Vec<usize> = (0..n).filter(|&i| !fixed[i]).collect();
    let mut fixed_total: f32 = (0..n).filter(|&i| fixed[i]).map(|i| w[i]).sum();
    // Percentages that add up to more than the space are scaled down to fit; if
    // every column is a percentage and they add up to less, they're scaled up to
    // fill the table (never below a column's min-content).
    if fixed_total > 0.0 && (fixed_total > avail || idx.is_empty()) {
        let k = avail / fixed_total;
        for i in 0..n {
            if fixed[i] {
                w[i] = (w[i] * k).max(min[i]);
            }
        }
        fixed_total = (0..n).filter(|&i| fixed[i]).map(|i| w[i]).sum();
    }
    let rest = (avail - fixed_total).max(0.0);
    let smin: f32 = idx.iter().map(|&i| min[i]).sum();
    let smax: f32 = idx.iter().map(|&i| max[i]).sum();
    for &i in &idx {
        w[i] = if rest >= smax {
            // everything fits at its natural width: share the extra in proportion to max
            if smax > 0.0 { max[i] + (rest - smax) * max[i] / smax } else { rest / idx.len() as f32 }
        } else if rest <= smin {
            min[i]
        } else {
            min[i] + (rest - smin) * (max[i] - min[i]) / (smax - smin)
        };
    }
    w
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

        // An empty block (no content, no height, padding or border) has no box to
        // speak of: its top and bottom margins collapse with each other and with
        // its neighbours' ("collapse through").
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
        self.emit_decor(s, bg_slot, bx, top, border_w, bottom, bw, None);
        (bottom, eff_mb)
    }

    /// Fill the background placeholder at `bg_slot` and push the border rects.
    /// `color_override` replaces the border colour (legacy `<table border>`).
    #[allow(clippy::too_many_arguments)]
    fn emit_decor(
        &mut self,
        s: &Style,
        bg_slot: usize,
        bx: f32,
        top: f32,
        border_w: f32,
        bottom: f32,
        bw: [f32; 4],
        color_override: Option<Color>,
    ) -> Decor {
        if let Some(color) = s.background {
            self.items[bg_slot] = Item::Rect { x: bx, y: top, w: border_w, h: bottom - top, color };
        }
        // None = transparent border: it still takes space but draws nothing
        let bc = |i: usize| match (color_override, s.border_c[i]) {
            (Some(c), _) => Some(c),
            (None, None) => Some(s.color),
            (None, Some(c)) => c,
        };
        let mut slots = [None; 4];
        let sides = [
            (bx, top, border_w, bw[0]),
            (bx + border_w - bw[1], top, bw[1], bottom - top),
            (bx, bottom - bw[2], border_w, bw[2]),
            (bx, top, bw[3], bottom - top),
        ];
        for (i, (x, y, w, h)) in sides.into_iter().enumerate() {
            if let (true, Some(color)) = (bw[i] > 0.0, bc(i)) {
                slots[i] = Some(self.items.len());
                self.items.push(Item::Rect { x, y, w, h, color });
            }
        }
        Decor { bg_slot, top, bw, border_slots: slots }
    }

    /// Make a laid-out box taller: extend its background and side borders, and
    /// move its bottom border down.
    fn stretch_decor(&mut self, d: &Decor, new_bottom: f32) {
        if let Item::Rect { h, .. } = &mut self.items[d.bg_slot] {
            *h = new_bottom - d.top;
        }
        for (i, slot) in d.border_slots.iter().enumerate() {
            let Some(ix) = slot else { continue };
            if let Item::Rect { y, h, .. } = &mut self.items[*ix] {
                match i {
                    2 => *y = new_bottom - d.bw[2],
                    1 | 3 => *h = new_bottom - d.top,
                    _ => {}
                }
            }
        }
    }

    /// Move a range of already-emitted content down by `dy`.
    fn shift(&mut self, items: (usize, usize), links: (usize, usize), controls: (usize, usize), dy: f32) {
        for it in &mut self.items[items.0..items.1] {
            match it {
                Item::Rect { y, .. } | Item::Image { y, .. } => *y += dy,
                Item::Text { baseline, .. } => *baseline += dy,
                Item::Nop => {}
            }
        }
        for l in &mut self.links[links.0..links.1] {
            l.y += dy;
        }
        for c in &mut self.controls[controls.0..controls.1] {
            c.y += dy;
        }
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
                    let collapsed = collapsed_child == Some(c);
                    let (b, mb) = if d == Display::Table {
                        self.layout_table(c, x, cursor, prev_mb, width, collapsed, id)
                    } else {
                        let mut marker = None;
                        if d == Display::ListItem {
                            counter += 1;
                            if !self.styles[id].list_none && !self.styles[c].list_none {
                                marker = Some(if is_ol { format!("{counter}.") } else { "•".to_string() });
                            }
                        }
                        self.layout_block(c, x, cursor, prev_mb, width, marker, collapsed)
                    };
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
        // a table's own margin is all there is: its rows don't collapse into it
        if s.display == Display::Table {
            return mt;
        }
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
                    // Form controls are drawn by the engine; their children (a
                    // <button>'s label, <option>s, <textarea> text) aren't flowed.
                    "input" | "button" | "textarea" | "select" => {
                        if let Some((w, asc, desc)) = self.control_metrics(id) {
                            out.push(Inl::Control { node: id, w, asc, desc });
                        }
                    }
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

    // ---------------------------------------------------------------- form controls

    /// (width, ascent, descent) of a control's box relative to the text baseline,
    /// or None for controls that draw nothing (`type=hidden`).
    fn control_metrics(&self, id: NodeId) -> Option<(f32, f32, f32)> {
        let kind = forms::kind_of(self.dom, id)?;
        let face = ctl_face();
        let (a, d) = self.fonts.vmetrics(face, CTL_FONT);
        let ch = self.fonts.measure(face, CTL_FONT, "0");
        // a legacy numeric attribute (`size`, `cols`, `rows`), at least 1
        let num = |name: &str, default: f32, max: f32| self.attr_num(id, name).filter(|n| *n >= 1.0).unwrap_or(default).min(max);
        Some(match kind {
            ControlKind::Hidden => return None,
            ControlKind::Text | ControlKind::Password => (num("size", 20.0, 100.0) * ch + 10.0, 3.0 + a, d + 3.0),
            ControlKind::Textarea => {
                let rows = num("rows", 2.0, 30.0).round();
                // a textarea's baseline is its bottom edge
                (num("cols", 20.0, 100.0) * ch + 10.0, rows * (a + d) + 6.0, 0.0)
            }
            ControlKind::Checkbox | ControlKind::Radio => (13.0, 13.0, 0.0),
            ControlKind::Submit | ControlKind::Button => {
                (self.fonts.measure(face, CTL_FONT, &self.control_label(id)) + 20.0, 4.0 + a, d + 4.0)
            }
            ControlKind::Select => {
                let widest = forms::options(self.dom, id)
                    .iter()
                    .map(|(_, label)| self.fonts.measure(face, CTL_FONT, label))
                    .fold(0.0f32, f32::max);
                (widest + 30.0, 3.0 + a, d + 3.0)
            }
        })
    }

    /// The text on a button.
    fn control_label(&self, id: NodeId) -> String {
        if self.dom.tag(id) == Some("button") {
            let text = self.dom.text_content(id).split_whitespace().collect::<Vec<_>>().join(" ");
            if !text.is_empty() {
                return text;
            }
        }
        let value = self.form.value(self.dom, id);
        if !value.is_empty() && self.dom.tag(id) != Some("button") {
            return value;
        }
        let ty = self.dom.attr(id, "type").unwrap_or("").trim().to_ascii_lowercase();
        match (forms::kind_of(self.dom, id), ty.as_str()) {
            (_, "reset") => "Reset".into(),
            (Some(ControlKind::Submit), _) => "Submit".into(),
            _ => String::new(),
        }
    }

    /// As much of `text` as fits in `max_w`: from the start, or (`tail`) the end,
    /// which keeps the caret visible while typing in a long field.
    fn clip(&self, text: &str, max_w: f32, tail: bool) -> String {
        let face = ctl_face();
        let (mut out, mut used) = (Vec::new(), 0.0f32);
        let chars: Box<dyn Iterator<Item = char>> = if tail { Box::new(text.chars().rev()) } else { Box::new(text.chars()) };
        for c in chars {
            let cw = self.fonts.measure(face, CTL_FONT, c.encode_utf8(&mut [0; 4]));
            if used + cw > max_w {
                break;
            }
            used += cw;
            out.push(c);
        }
        if tail {
            out.reverse();
        }
        out.into_iter().collect()
    }

    /// A bordered box: `border` all round, `fill` inside.
    #[allow(clippy::too_many_arguments)]
    fn push_box(&mut self, x: f32, y: f32, w: f32, h: f32, bw: f32, border: Color, fill: Color) {
        self.items.push(Item::Rect { x, y, w, h, color: border });
        self.items.push(Item::Rect { x: x + bw, y: y + bw, w: (w - 2.0 * bw).max(0.0), h: (h - 2.0 * bw).max(0.0), color: fill });
    }

    fn push_text(&mut self, x: f32, baseline: f32, text: String, size: f32, color: Color) {
        if !text.is_empty() {
            self.items.push(Item::Text { x, baseline, text, size, face: ctl_face(), color });
        }
    }

    /// Draw a form control whose box has its text baseline at `baseline`.
    fn emit_control(&mut self, id: NodeId, x: f32, baseline: f32, w: f32, asc: f32, desc: f32) {
        let Some(kind) = forms::kind_of(self.dom, id) else { return };
        let (top, h) = (baseline - asc, asc + desc);
        let (a, d) = self.fonts.vmetrics(ctl_face(), CTL_FONT);
        let focused = self.form.focus == Some(id);
        let disabled = forms::is_disabled(self.dom, id);
        let (border, bw) = if focused { (CTL_FOCUS, 2.0) } else { (CTL_BORDER, 1.0) };
        let ink = if disabled { CTL_DISABLED } else { CTL_TEXT };

        match kind {
            ControlKind::Hidden => return,
            ControlKind::Checkbox | ControlKind::Radio => {
                self.push_box(x, top, w, h, bw, border, Color::WHITE);
                if self.form.is_checked(self.dom, id) {
                    let inset = if kind == ControlKind::Radio { 4.0 } else { 3.0 };
                    self.items.push(Item::Rect { x: x + inset, y: top + inset, w: w - 2.0 * inset, h: h - 2.0 * inset, color: ink });
                }
            }
            ControlKind::Submit | ControlKind::Button => {
                self.push_box(x, top, w, h, bw, border, CTL_FACE);
                let label = self.control_label(id);
                let tw = self.fonts.measure(ctl_face(), CTL_FONT, &label);
                self.push_text(x + (w - tw) / 2.0, baseline, label, CTL_FONT, ink);
            }
            ControlKind::Text | ControlKind::Password => {
                self.push_box(x, top, w, h, bw, border, Color::WHITE);
                let value = self.form.value(self.dom, id);
                let shown = if kind == ControlKind::Password { "•".repeat(value.chars().count()) } else { value };
                let inner = w - 10.0;
                let (text, color) = if shown.is_empty() {
                    (self.clip(self.dom.attr(id, "placeholder").unwrap_or(""), inner, false), CTL_PLACEHOLDER)
                } else {
                    (self.clip(&shown, inner, focused), ink)
                };
                let tw = self.fonts.measure(ctl_face(), CTL_FONT, &text);
                self.push_text(x + 5.0, baseline, text, CTL_FONT, color);
                if focused {
                    // the caret sits after the typed text, not after placeholder text
                    let caret_x = x + 5.0 + if shown.is_empty() { 0.0 } else { tw };
                    self.items.push(Item::Rect { x: caret_x, y: top + 3.0, w: 1.0, h: a + d, color: CTL_TEXT });
                }
            }
            ControlKind::Textarea => {
                self.push_box(x, top, w, h, bw, border, Color::WHITE);
                let value = self.form.value(self.dom, id);
                let line_h = a + d;
                let rows = (((h - 6.0) / line_h).floor() as usize).max(1);
                let inner = w - 10.0;
                let (lines, color): (Vec<String>, Color) = if value.is_empty() {
                    (vec![self.dom.attr(id, "placeholder").unwrap_or("").to_string()], CTL_PLACEHOLDER)
                } else {
                    (value.split('\n').map(String::from).collect(), ink)
                };
                // show the last `rows` lines, so what's being typed stays visible
                let skip = lines.len().saturating_sub(rows);
                let last = lines.len() - 1;
                for (i, line) in lines.iter().skip(skip).enumerate() {
                    let b = top + 3.0 + a + i as f32 * line_h;
                    let clipped = self.clip(line, inner, focused && skip + i == last);
                    if focused && skip + i == last && !value.is_empty() {
                        let tw = self.fonts.measure(ctl_face(), CTL_FONT, &clipped);
                        self.items.push(Item::Rect { x: x + 5.0 + tw, y: b - a, w: 1.0, h: line_h, color: CTL_TEXT });
                    }
                    self.push_text(x + 5.0, b, clipped, CTL_FONT, color);
                }
                if focused && value.is_empty() {
                    self.items.push(Item::Rect { x: x + 5.0, y: top + 3.0, w: 1.0, h: line_h, color: CTL_TEXT });
                }
            }
            ControlKind::Select => {
                self.push_box(x, top, w, h, bw, border, Color::WHITE);
                let label = self.clip(&self.form.selected_label(self.dom, id), w - 25.0, false);
                self.push_text(x + 5.0, baseline, label, CTL_FONT, ink);
                self.push_text(x + w - 16.0, baseline, "▼".to_string(), 8.0, ink);
            }
        }
        self.controls.push(ControlRegion { x, y: top, w, h, node: id });
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
            // Everything placeable (a word, an image or a control) reduces to the same tuple.
            let (text, node, w, a, d, pre, image_h, control) = match it {
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
                    (text.clone(), *node, fonts.measure(s.face(), s.font_size, text), a, d, *pre, None, None)
                }
                Inl::Image { node, w, h } => (String::new(), *node, *w, *h, 0.0, false, Some(*h), None),
                Inl::Control { node, w, asc, desc } => (String::new(), *node, *w, *asc, *desc, false, None, Some((*asc, *desc))),
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
            line.words.push(Placed { text, node, x: cx + sp, w, href, image_h, control });
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
                if let Some((asc, desc)) = w.control {
                    self.emit_control(w.node, ax, baseline, w.w, asc, desc);
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

    // ---------------------------------------------------------------- intrinsic widths

    /// (min-content, max-content) width of the inline content in `items`: the
    /// widest unbreakable word, and the width of the longest line left unwrapped.
    fn inline_widths(&self, items: &[Inl]) -> (f32, f32) {
        let (mut min, mut max, mut line) = (0.0f32, 0.0f32, 0.0f32);
        let mut space: Option<NodeId> = None;
        for it in items {
            let w = match it {
                Inl::Space(n) => {
                    space = Some(*n);
                    continue;
                }
                Inl::Break(_) => {
                    max = max.max(line);
                    line = 0.0;
                    space = None;
                    continue;
                }
                Inl::Word { text, node, .. } => {
                    let s = &self.styles[*node];
                    self.fonts.measure(s.face(), s.font_size, text)
                }
                Inl::Image { w, .. } | Inl::Control { w, .. } => *w,
            };
            if line > 0.0 {
                if let Some(sn) = space {
                    let ss = &self.styles[sn];
                    line += self.fonts.measure(ss.face(), ss.font_size, " ");
                }
            }
            line += w;
            min = min.max(w);
            space = None;
        }
        (min, max.max(line))
    }

    /// (min, max) content width of a block's contents (not including its own
    /// padding, border or margin).
    fn content_widths(&self, id: NodeId) -> (f32, f32) {
        let (mut min, mut max) = (0.0f32, 0.0f32);
        let mut run: Vec<NodeId> = vec![];
        let flush = |this: &Self, run: &mut Vec<NodeId>, min: &mut f32, max: &mut f32| {
            if run.is_empty() {
                return;
            }
            let mut items = vec![];
            for &n in run.iter() {
                this.flatten(n, &mut items);
            }
            run.clear();
            let (a, b) = this.inline_widths(&items);
            *min = min.max(a);
            *max = max.max(b);
        };
        for c in self.flow_children(id) {
            match self.styles[c].display {
                Display::None => {}
                Display::Inline => run.push(c),
                _ => {
                    flush(self, &mut run, &mut min, &mut max);
                    let (a, b) = self.outer_widths(c);
                    min = min.max(a);
                    max = max.max(b);
                }
            }
        }
        flush(self, &mut run, &mut min, &mut max);
        (min, max)
    }

    /// (min, max) outer width of a block-level child: contents plus its own
    /// padding, border and (fixed) margins. A fixed CSS `width` overrides the contents.
    fn outer_widths(&self, id: NodeId) -> (f32, f32) {
        let s = &self.styles[id];
        if s.display == Display::Table {
            return self.table_widths(id);
        }
        let (mut min, mut max) = self.content_widths(id);
        if let Length::Px(w) = s.width {
            min = w.max(0.0);
            max = min;
        }
        let px = |l: Length| resolve(l, 0.0).unwrap_or(0.0);
        let bw = s.borders();
        let extras = px(s.padding[1]).max(0.0) + px(s.padding[3]).max(0.0) + bw[1] + bw[3]
            + px(s.margin[1]) + px(s.margin[3]);
        ((min + extras).max(0.0), (max + extras).max(0.0))
    }

    // ---------------------------------------------------------------- tables

    /// A legacy numeric attribute (`cellpadding="4"`), clamped to something sane.
    fn attr_num(&self, id: NodeId, name: &str) -> Option<f32> {
        let v = self.dom.attr(id, name)?;
        let digits: String = v.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
        digits.parse::<f32>().ok().map(|n| n.clamp(0.0, 100.0))
    }

    /// The legacy `border` attribute: `border=2` -> 2px, bare `border` -> 1px.
    fn html_border(&self, table: NodeId) -> f32 {
        match self.dom.attr(table, "border") {
            Some(v) if v.trim().is_empty() => 1.0,
            Some(_) => self.attr_num(table, "border").unwrap_or(0.0),
            None => 0.0,
        }
    }

    fn table_spacing(&self, table: NodeId) -> f32 {
        let s = &self.styles[table];
        if s.collapse {
            return 0.0;
        }
        match (s.spacing, self.attr_num(table, "cellspacing")) {
            // 2px is the user-agent default, so a `cellspacing` attribute overrides it
            (Some(v), Some(a)) if v == 2.0 => a,
            (Some(v), _) => v,
            (None, Some(a)) => a,
            (None, None) => 2.0,
        }
    }

    fn colspan(&self, cell: NodeId) -> usize {
        self.dom.attr(cell, "colspan").and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(1).clamp(1, MAX_COLS)
    }

    /// `rowspan`: 1 by default; 0 means "to the end of the table".
    fn rowspan(&self, cell: NodeId) -> usize {
        match self.dom.attr(cell, "rowspan").and_then(|v| v.trim().parse::<usize>().ok()) {
            None => 1,
            Some(0) => usize::MAX,
            Some(n) => n,
        }
    }

    /// Padding, border widths and (for legacy `<table border>`) a border colour for a cell.
    fn cell_box(&self, table: NodeId, cell: NodeId) -> ([f32; 4], [f32; 4], Option<Color>) {
        let s = &self.styles[cell];
        let pad = match self.attr_num(table, "cellpadding") {
            Some(p) => [p; 4],
            None => s.padding.map(|l| resolve(l, 0.0).unwrap_or(0.0).max(0.0)),
        };
        let mut bw = s.borders();
        let mut color = None;
        if bw.iter().all(|&w| w == 0.0) && self.html_border(table) > 0.0 {
            bw = [1.0; 4];
            color = Some(Color { r: 128, g: 128, b: 128 });
        }
        // `border-collapse: collapse` approximation: neighbouring cells each draw
        // half of the shared edge, so interior lines come out at the full width
        // instead of double.
        if self.styles[table].collapse {
            bw = bw.map(|w| w / 2.0);
        }
        (pad, bw, color)
    }

    /// (min, max, percentage) outer width of a cell.
    fn cell_widths(&self, table: NodeId, cell: NodeId) -> (f32, f32, Option<f32>) {
        let (pad, bw, _) = self.cell_box(table, cell);
        let extras = pad[1] + pad[3] + bw[1] + bw[3];
        let (cmin, cmax) = self.content_widths(cell);
        let (min, mut max) = (cmin + extras, cmax + extras);
        let mut pct = None;
        match self.styles[cell].width {
            Length::Px(w) => max = (w.max(0.0) + extras).max(min),
            Length::Pct(p) => pct = Some(p.clamp(0.0, 100.0)),
            Length::Auto => {}
        }
        (min, max.max(min), pct)
    }

    fn build_table(&self, id: NodeId) -> TableModel {
        let mut row_ids: Vec<(NodeId, Option<NodeId>)> = vec![];
        let mut captions = vec![];
        for c in self.flow_children(id) {
            match self.styles[c].display {
                Display::TableRow => row_ids.push((c, None)),
                Display::TableRowGroup => {
                    for r in self.flow_children(c) {
                        if self.styles[r].display == Display::TableRow {
                            row_ids.push((r, Some(c)));
                        }
                    }
                }
                Display::Block if self.dom.tag(c) == Some("caption") => captions.push(c),
                _ => {}
            }
        }
        let spacing = self.table_spacing(id);
        let nrows = row_ids.len();
        let mut rows = vec![];
        // occupied[c] = the first row index at which column c is free again: a
        // cell with rowspan > 1 keeps its columns busy in the rows below it.
        let mut occupied: Vec<usize> = vec![];
        for (ri, (r, group)) in row_ids.into_iter().enumerate() {
            let (mut col, mut cells) = (0, vec![]);
            for cc in self.flow_children(r) {
                if self.styles[cc].display != Display::TableCell {
                    continue;
                }
                while col < occupied.len() && occupied[col] > ri {
                    col += 1;
                }
                if col >= MAX_COLS {
                    break;
                }
                let span = self.colspan(cc).min(MAX_COLS - col);
                let rowspan = self.rowspan(cc).min(nrows - ri).max(1);
                if occupied.len() < col + span {
                    occupied.resize(col + span, 0);
                }
                for o in &mut occupied[col..col + span] {
                    *o = ri + rowspan;
                }
                cells.push(Cell { id: cc, col, span, rowspan });
                col += span;
            }
            rows.push(RowInfo { id: r, group, cells });
        }
        let ncols = occupied.len();

        let (mut min, mut max, mut pct) = (vec![0.0f32; ncols], vec![0.0f32; ncols], vec![None; ncols]);
        // single-column cells set the baseline; spanning cells then top up what they need
        for pass in 0..2 {
            for row in &rows {
                for cell in &row.cells {
                    if (pass == 0) != (cell.span == 1) {
                        continue;
                    }
                    let (cmin, cmax, cpct) = self.cell_widths(id, cell.id);
                    let c = cell.col;
                    if cell.span == 1 {
                        min[c] = min[c].max(cmin);
                        max[c] = max[c].max(cmax);
                        if let Some(p) = cpct {
                            pct[c] = Some(pct[c].map_or(p, |q: f32| q.max(p)));
                        }
                    } else {
                        let gaps = spacing * (cell.span as f32 - 1.0);
                        for (need, cols) in [(cmin, &mut min), (cmax, &mut max)] {
                            let need = (need - gaps).max(0.0);
                            let have: f32 = cols[c..c + cell.span].iter().sum();
                            if need > have {
                                let extra = (need - have) / cell.span as f32;
                                for v in &mut cols[c..c + cell.span] {
                                    *v += extra;
                                }
                            }
                        }
                    }
                }
            }
        }
        for i in 0..ncols {
            max[i] = max[i].max(min[i]);
        }

        let s = &self.styles[id];
        let html_border = self.html_border(id);
        let mut bw = s.borders();
        if bw.iter().all(|&w| w == 0.0) && html_border > 0.0 {
            bw = [html_border; 4];
        }
        TableModel { rows, captions, min, max, pct, spacing, bw, html_border: html_border > 0.0 }
    }

    /// The table's (min, max) outer width taking its captions into account: a
    /// caption is as wide as the table, so its longest word is a lower bound.
    fn table_extent(&self, m: &TableModel) -> (f32, f32) {
        let chrome = m.spacing * (m.min.len() as f32 + 1.0) + m.bw[1] + m.bw[3];
        let (mut min_w, mut max_w) = (m.min.iter().sum::<f32>() + chrome, m.max.iter().sum::<f32>() + chrome);
        for cap in &m.captions {
            let (cmin, cmax) = self.outer_widths(*cap);
            min_w = min_w.max(cmin);
            max_w = max_w.max(cmax);
        }
        (min_w, max_w.max(min_w))
    }

    /// (min, max) outer width of a whole table.
    fn table_widths(&self, id: NodeId) -> (f32, f32) {
        let m = self.build_table(id);
        let (mut min, mut max) = self.table_extent(&m);
        if let Length::Px(w) = self.styles[id].width {
            min = min.max(w);
            max = min;
        }
        (min, max.max(min))
    }

    /// Lay out one cell as a box of outer width `outer_w` at (x, y).
    fn layout_cell(&mut self, table: NodeId, id: NodeId, x: f32, y: f32, outer_w: f32) -> CellOut {
        let styles = self.styles;
        let s = &styles[id];
        let (pad, bw, color) = self.cell_box(table, id);
        let content_w = (outer_w - pad[1] - pad[3] - bw[1] - bw[3]).max(0.0);
        let bg_slot = self.items.len();
        self.items.push(Item::Nop);
        let (content_start, links_start, controls_start) = (self.items.len(), self.links.len(), self.controls.len());

        let (end_y, last_mb) = self.layout_children(id, x + bw[3] + pad[3], y + bw[0] + pad[0], content_w, false);
        let content = (content_start, self.items.len());
        let links = (links_start, self.links.len());
        let controls = (controls_start, self.controls.len());

        // a cell is its own formatting context: its last margin stays inside it
        let inner_h = (end_y + last_mb) - (y + bw[0] + pad[0]);
        let mut height = inner_h + pad[0] + pad[2] + bw[0] + bw[2];
        if let Some(h) = s.height {
            height = height.max(h + pad[0] + pad[2] + bw[0] + bw[2]);
        }
        let decor = self.emit_decor(s, bg_slot, x, y, outer_w, y + height, bw, color);
        CellOut { height, decor, content, links, controls }
    }

    /// Vertically align a laid-out cell inside the box `[top, bottom]` and stretch
    /// its background and borders to fill it.
    fn place_cell(&mut self, out: &CellOut, valign: VAlign, top: f32, bottom: f32) {
        let free = (bottom - top) - out.height;
        let dy = match valign {
            VAlign::Top => 0.0,
            VAlign::Middle => free / 2.0,
            VAlign::Bottom => free,
        };
        if dy != 0.0 {
            self.shift(out.content, out.links, out.controls, dy);
        }
        self.stretch_decor(&out.decor, bottom);
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_table(
        &mut self,
        id: NodeId,
        left: f32,
        cursor: f32,
        prev_mb: f32,
        avail: f32,
        top_collapsed: bool,
        parent: NodeId,
    ) -> (f32, f32) {
        let styles = self.styles;
        let s = &styles[id];
        let mt = resolve(s.margin[0], avail).unwrap_or(0.0);
        let mb = resolve(s.margin[2], avail).unwrap_or(0.0);
        let (ml_opt, mr_opt) = (resolve(s.margin[3], avail), resolve(s.margin[1], avail));

        let model = self.build_table(id);
        if model.rows.is_empty() && model.captions.is_empty() {
            let merged = if top_collapsed { mb } else { collapse(prev_mb, collapse(mt, mb)) };
            return (cursor, merged);
        }
        let n = model.min.len();
        let (spacing, bw) = (model.spacing, model.bw);
        let chrome = spacing * (n as f32 + 1.0) + bw[1] + bw[3];
        let (min_w, max_w) = self.table_extent(&model);

        // Table width: an explicit width (never below the min-content), otherwise
        // shrink-to-fit: as wide as the contents want, but no wider than the space.
        let pref = resolve(s.width, avail);
        let mut table_w = match pref {
            Some(w) => w.max(min_w),
            None => max_w.min(avail).max(min_w),
        };
        if let Some(mw) = s.max_width {
            table_w = table_w.min(mw.max(min_w));
        }
        let widths = distribute(&model.min, &model.max, &model.pct, (table_w - chrome).max(0.0));
        let table_w = widths.iter().sum::<f32>() + chrome;

        // Horizontal position: auto margins, the legacy `align` attribute, or a <center> parent.
        let free = (avail - table_w).max(0.0);
        let ml = match (ml_opt, mr_opt) {
            (None, None) => free / 2.0,
            (None, Some(r)) => (free - r).max(0.0),
            (Some(l), _) if l != 0.0 => l,
            _ => match self.dom.attr(id, "align").map(|a| a.trim().to_ascii_lowercase()).as_deref() {
                Some("center") => free / 2.0,
                Some("right") => free,
                _ if self.dom.tag(parent) == Some("center") => free / 2.0,
                _ => ml_opt.unwrap_or(0.0),
            },
        };
        let bx = left + ml;
        let top = if top_collapsed { cursor } else { cursor + collapse(prev_mb, mt) };
        let bg_slot = self.items.len();
        self.items.push(Item::Nop);

        let content_x = bx + bw[3];
        let mut y = top + bw[0];
        for cap in &model.captions {
            y = self.layout_block(*cap, bx, y, 0.0, table_w, None, true).0;
        }

        let nrows = model.rows.len();
        let mut row_top = vec![0.0f32; nrows];
        let mut row_h = vec![0.0f32; nrows];
        let mut spanning: Vec<Spanning> = vec![];
        let mut y_row = y + spacing;
        for (ri, row) in model.rows.iter().enumerate() {
            let rs = &styles[row.id];
            // Reserve a slot for the row's background now, so nothing is ever inserted
            // into the middle of the display list (which would shift recorded indices).
            let row_bg_slot = self.items.len();
            self.items.push(Item::Nop);
            row_top[ri] = y_row;

            let mut outs: Vec<(usize, CellOut)> = vec![]; // (index into row.cells, laid-out cell)
            let mut h = 0.0f32;
            let (mut x_min, mut x_max) = (f32::MAX, f32::MIN);
            for (ci, cell) in row.cells.iter().enumerate() {
                let x = content_x + spacing + widths[..cell.col].iter().sum::<f32>() + spacing * cell.col as f32;
                let w = widths[cell.col..cell.col + cell.span].iter().sum::<f32>() + spacing * (cell.span as f32 - 1.0);
                x_min = x_min.min(x);
                x_max = x_max.max(x + w);
                let out = self.layout_cell(id, cell.id, x, y_row, w);
                if cell.rowspan > 1 {
                    let valign = styles[cell.id].valign.or(rs.valign).or(row.group.and_then(|g| styles[g].valign)).unwrap_or(VAlign::Middle);
                    spanning.push(Spanning { out, first: ri, last: ri + cell.rowspan - 1, valign });
                } else {
                    h = h.max(out.height);
                    outs.push((ci, out));
                }
            }
            if let Some(rh) = rs.height {
                h = h.max(rh);
            }
            // A spanning cell that ends in this row may need more height than its rows
            // have between them: the last row grows to fit it.
            for sp in spanning.iter().filter(|sp| sp.last == ri) {
                let above: f32 = row_h[sp.first..ri].iter().sum::<f32>() + spacing * (ri - sp.first) as f32;
                let deficit = sp.out.height - (above + h);
                if deficit > 0.0 {
                    h += deficit;
                }
            }
            row_h[ri] = h;
            let bottom = y_row + h;

            for (ci, out) in &outs {
                let cell = &row.cells[*ci];
                let valign = styles[cell.id].valign.or(rs.valign).or(row.group.and_then(|g| styles[g].valign)).unwrap_or(VAlign::Middle);
                self.place_cell(out, valign, y_row, bottom);
            }
            let finished: Vec<Spanning> = {
                let (done, rest): (Vec<_>, Vec<_>) = spanning.into_iter().partition(|sp| sp.last == ri);
                spanning = rest;
                done
            };
            for sp in finished {
                self.place_cell(&sp.out, sp.valign, row_top[sp.first], bottom);
            }
            // a row's own background sits behind its cells
            if let (Some(color), true) = (rs.background, x_min <= x_max) {
                self.items[row_bg_slot] = Item::Rect { x: x_min, y: y_row, w: x_max - x_min, h, color };
            }
            y_row += h + spacing;
        }

        let bottom = y_row + bw[2];
        let color = if model.html_border { Some(Color { r: 128, g: 128, b: 128 }) } else { None };
        self.emit_decor(s, bg_slot, bx, top, table_w, bottom, bw, color);
        (bottom, mb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use url::Url;

    fn render_w(body: &str, width: f32) -> Page {
        let fonts = Fonts::load();
        let src = format!("<style>body{{margin:0}}</style>{body}");
        let mut p = Page::from_source(&Url::parse("file:///tmp/t.html").unwrap(), &src);
        p.relayout(&fonts, width);
        p
    }

    fn render(body: &str) -> Page {
        render_w(body, 400.0)
    }

    fn texts_full(p: &Page) -> Vec<(String, f32, f32)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Text { text, x, baseline, .. } => Some((text.clone(), *x, *baseline)),
            _ => None,
        }).collect()
    }

    fn texts(p: &Page) -> Vec<(String, f32)> {
        texts_full(p).into_iter().map(|(t, _, b)| (t, b)).collect()
    }

    fn at(p: &Page, word: &str) -> (f32, f32) {
        let (_, x, b) = texts_full(p).into_iter().find(|(t, _, _)| t == word).unwrap_or_else(|| panic!("no text {word}"));
        (x, b)
    }

    fn rects(p: &Page) -> Vec<(f32, f32, f32, f32, Color)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Rect { x, y, w, h, color } => Some((*x, *y, *w, *h, *color)),
            _ => None,
        }).collect()
    }

    const RED: Color = Color { r: 255, g: 0, b: 0 };

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
        let red = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
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

    // ---------------------------------------------------------------- tables

    #[test]
    fn a_table_lays_cells_out_in_columns_and_rows() {
        let p = render("<table><tr><td>one</td><td>two</td></tr><tr><td>three</td><td>four</td></tr></table>");
        let (one, two, three, four) = (at(&p, "one"), at(&p, "two"), at(&p, "three"), at(&p, "four"));
        assert_eq!(one.1, two.1, "cells of a row share a baseline");
        assert_eq!(three.1, four.1);
        assert!(three.1 > one.1 + 10.0, "the second row is below the first");
        assert!(two.0 > one.0 + 10.0, "the second column is to the right");
        assert!((one.0 - three.0).abs() < 0.01 && (two.0 - four.0).abs() < 0.01, "columns line up");
        assert!(two.0 < 150.0, "an auto-width table is shrink-to-fit, not full width");
    }

    #[test]
    fn column_widths_follow_content_and_table_width() {
        // 100% width: the wordy column gets the extra space, the short one stays narrow
        let p = render("<table width=100%><tr><td>a</td><td>a much longer cell of text that wants room</td></tr></table>");
        let long = at(&p, "much");
        assert!(long.0 < 80.0, "the short column stays narrow (long cell starts at {})", long.0);
        // a table in a narrow container wraps text but never goes below the longest word
        let p = render_w("<table><tr><td>supercalifragilistic and more words</td></tr></table>", 50.0);
        let r = rects(&render_w("<table style='background:red'><tr><td>supercalifragilistic and more words</td></tr></table>", 50.0));
        let table = r.iter().find(|r| r.4 == RED).unwrap();
        assert!(table.2 > 100.0, "min-content (the long word) wins over a 50px container: {}", table.2);
        assert!(texts(&p).len() >= 4);
        // fixed column widths from the legacy width attribute
        let p = render("<table><tr><td width=200>x</td><td>y</td></tr></table>");
        assert!(at(&p, "y").0 > 200.0);
        // percentage columns
        let p = render_w("<table width=100%><tr><td width=75%>x</td><td>y</td></tr></table>", 400.0);
        assert!((at(&p, "y").0 - 300.0).abs() < 8.0, "75% column puts the next at ~300, got {}", at(&p, "y").0);
    }

    #[test]
    fn colspan_cells_span_columns() {
        let p = render("<table width=300 style='border-spacing:0'><tr><td colspan=2 align=center>HEAD</td></tr><tr><td>left</td><td>right</td></tr></table>");
        let (head, left) = (at(&p, "HEAD"), at(&p, "left"));
        // centred across both columns: the ~44px-wide word starts near 150 - 22
        assert!(head.0 > left.0 + 20.0 && (head.0 - 128.0).abs() < 12.0, "heading at {head:?}, left cell at {left:?}");
        // a colspan wider than the table's columns still gets the room it needs
        let p = render("<table><tr><td colspan=2>a quite long heading spanning both</td></tr><tr><td>x</td><td>y</td></tr></table>");
        assert!(at(&p, "y").0 > 20.0);
    }

    #[test]
    fn cell_backgrounds_and_borders_stretch_to_the_row_height() {
        let p = render("<table style='border-spacing:0'><tr><td style='background:red;padding:0'>a</td><td style='padding:0'>b<br>b<br>b</td></tr></table>");
        let red = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
        let tall = at(&p, "b");
        let (_, last_b) = texts(&p).into_iter().filter(|(t, _)| t == "b").last().unwrap();
        assert!(red.3 > (last_b - tall.1), "red covers the whole three-line row: h={} span={}", red.3, last_b - tall.1);
        // legacy border attribute draws a border box around every cell
        let p = render("<table border=1><tr><td>a</td><td>b</td></tr></table>");
        let borders = rects(&p).len();
        assert!(borders >= 4 + 2 * 4, "table + 2 cells have borders: {borders}");
    }

    #[test]
    fn cells_are_vertically_centred_unless_told_otherwise() {
        let tall = "<td>t<br>t<br>t</td>";
        let mid = render(&format!("<table><tr><td>m</td>{tall}</tr></table>"));
        let top = render(&format!("<table><tr valign=top><td>m</td>{tall}</tr></table>"));
        let bottom = render(&format!("<table><tr><td valign=bottom>m</td>{tall}</tr></table>"));
        let first_t = |p: &Page| texts(p).into_iter().find(|(t, _)| t == "t").unwrap().1;
        let m = |p: &Page| at(p, "m").1;
        assert_eq!(m(&top), first_t(&top), "valign=top aligns with the first line");
        assert!(m(&mid) > first_t(&mid) + 5.0, "middle sits lower than the first line");
        assert!(m(&bottom) > m(&mid), "bottom sits lowest");
        let css = render(&format!("<table><tr><td style='vertical-align:top'>m</td>{tall}</tr></table>"));
        assert_eq!(m(&css), first_t(&css));
    }

    #[test]
    fn captions_row_backgrounds_cellspacing_and_centering() {
        let p = render("<table><caption>Title</caption><tr><td>cell</td></tr></table>");
        assert!(at(&p, "Title").1 < at(&p, "cell").1, "the caption is above the rows");
        // a row's bgcolor paints one rect behind its cells
        let p = render("<table><tr bgcolor=red><td>a</td><td>b</td></tr></table>");
        let red = rects(&p).into_iter().find(|r| r.4 == RED).expect("row background");
        assert!(red.2 > 20.0);
        // cellspacing and cellpadding attributes
        let tight = render("<table cellspacing=0 cellpadding=0><tr><td>x</td></tr></table>");
        let loose = render("<table cellspacing=10 cellpadding=10><tr><td>x</td></tr></table>");
        assert!(loose.layout.height > tight.layout.height + 30.0);
        assert!(at(&loose, "x").0 > at(&tight, "x").0 + 15.0);
        // <center> and align=center centre a shrink-to-fit table
        let c = render("<center><table><tr><td>mid</td></tr></table></center>");
        assert!(at(&c, "mid").0 > 150.0, "centred in 400px: {}", at(&c, "mid").0);
        let a = render("<table align=center><tr><td>mid</td></tr></table>");
        assert!(at(&a, "mid").0 > 150.0);
        let left = render("<table><tr><td>mid</td></tr></table>");
        assert!(at(&left, "mid").0 < 20.0);
    }

    #[test]
    fn nested_tables_lay_out_and_size_to_their_contents() {
        let p = render("<table><tr><td><table><tr><td>in</td><td>ner</td></tr></table></td><td>out</td></tr></table>");
        let (inner, ner, out) = (at(&p, "in"), at(&p, "ner"), at(&p, "out"));
        assert!(ner.0 > inner.0 && out.0 > ner.0, "inner then outer cells, left to right");
        // deep nesting stays fast and doesn't overflow the stack
        let n = 25;
        let body = format!("{}deep{}", "<table><tr><td>".repeat(n), "</td></tr></table>".repeat(n));
        let t = std::time::Instant::now();
        let p = render(&body);
        assert!(texts(&p).iter().any(|(s, _)| s == "deep"));
        assert!(t.elapsed().as_secs() < 5);
    }

    #[test]
    fn hostile_tables_do_not_panic_or_explode() {
        for body in [
            "<table></table>",
            "<table><tr></tr><tr></tr></table>",
            "<table><tr><td colspan=999999999>x</td><td>y</td></tr></table>",
            "<table><tr><td colspan=0>x</td><td colspan=-3>y</td><td colspan=abc>z</td></tr></table>",
            "<table style='width:-50px;border-spacing:99999px'><tr><td style='width:-5px;padding:-9px'>x</td></tr></table>",
            "<table cellpadding=99999 cellspacing=99999 border=99999><tr><td>x</td></tr></table>",
            "<table width=1%><tr><td>unbreakable_unbreakable_unbreakable</td></tr></table>",
            "<table><td>cell with no row</td></table>",
            "<table>stray text<tr><td>x</td></tr>more stray text</table>",
            "<tr><td>row with no table</td></tr>",
            "<table><tr><td><table></table></td></tr></table>",
            "<div style='display:table-cell'>x</div><div style='display:table-row'>y</div>",
            "<table><tr><td rowspan=999999999>x</td><td rowspan=0>y</td><td rowspan=-2>z</td><td rowspan=abc>w</td></tr><tr><td>q</td></tr></table>",
            "<table><tr><td rowspan=3 colspan=3>big</td></tr><tr></tr><tr><td>a</td></tr></table>",
        ] {
            let p = render(body);
            assert!(p.layout.height.is_finite(), "{body}");
        }
        // a wide table is capped at MAX_COLS columns
        let cells = "<td>x</td>".repeat(MAX_COLS + 500);
        let p = render(&format!("<table><tr>{cells}</tr></table>"));
        assert!(p.layout.height.is_finite());
        // many rows stay quick
        let rows = "<tr><td>row</td><td>data</td></tr>".repeat(3000);
        let t = std::time::Instant::now();
        let p = render(&format!("<table>{rows}</table>"));
        assert!(p.layout.height > 3000.0 * 10.0);
        assert!(t.elapsed().as_secs() < 5, "{:?}", t.elapsed());
    }

    #[test]
    fn distribute_shares_space_sensibly() {
        // everything fits: extra space is shared in proportion to max-content
        let w = distribute(&[10.0, 10.0], &[50.0, 150.0], &[None, None], 400.0);
        assert!((w.iter().sum::<f32>() - 400.0).abs() < 0.01);
        assert!(w[1] > w[0] * 2.5);
        // too narrow: columns collapse to their minimums
        assert_eq!(distribute(&[30.0, 40.0], &[100.0, 100.0], &[None, None], 10.0), vec![30.0, 40.0]);
        // in between: each column is between its min and max
        let w = distribute(&[10.0, 10.0], &[100.0, 100.0], &[None, None], 120.0);
        assert!(w.iter().all(|&v| (10.0..=100.0).contains(&v)) && (w[0] + w[1] - 120.0).abs() < 0.01);
        // percentages
        let w = distribute(&[10.0, 10.0], &[50.0, 50.0], &[Some(25.0), None], 200.0);
        assert_eq!((w[0], w[1]), (50.0, 150.0));
        // no columns at all
        assert!(distribute(&[], &[], &[], 100.0).is_empty());
    }

    #[test]
    fn pages_with_tables_paint_without_panicking() {
        use crate::paint::Painter;
        let fonts = Fonts::load();
        let p = render("<table border=1 width=100%><caption>c</caption><tr bgcolor=red><td>a</td><td rowspan=2>b</td></tr><tr><td>c</td></tr></table>");
        let (w, h) = (400usize, 200usize);
        Painter::default().paint(&mut vec![0; w * h], w, h, &p.layout, &fonts, 0.0, 2.0);
    }

    // ---------- regressions from the table review ----------

    #[test]
    fn cells_with_no_row_still_render() {
        let p = render("<table border=1><td>X</td><td>Y</td></table>");
        let (x, y) = (at(&p, "X"), at(&p, "Y"));
        assert_eq!(x.1, y.1);
        assert!(y.0 > x.0, "two cells side by side in one implied row");
    }

    #[test]
    fn rowspan_cells_span_rows_and_push_later_cells_over() {
        // A spans both rows, so C sits under B (column 1), and D opens a third column
        let p = render("<table style='border-spacing:0'><tr><td rowspan=2>A</td><td>B</td></tr><tr><td>C</td><td>D</td></tr></table>");
        let (a, b, c, d) = (at(&p, "A"), at(&p, "B"), at(&p, "C"), at(&p, "D"));
        assert!((c.0 - b.0).abs() < 0.01, "C is under B, not under A: {c:?} vs {b:?}");
        assert!(c.1 > b.1 + 10.0, "C is on the second row");
        assert!(d.0 > c.0 + 5.0, "D is in a third column");
        assert!(a.1 > b.1 && a.1 < c.1, "A is vertically centred between the two rows: {a:?} {b:?} {c:?}");
        assert!(a.0 < b.0);
    }

    #[test]
    fn rowspan_backgrounds_cover_all_spanned_rows_and_grow_rows_to_fit() {
        let p = render("<table style='border-spacing:0'><tr><td rowspan=2 style='background:red;padding:0'>a</td><td>B</td></tr><tr><td>C</td></tr></table>");
        let red = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
        let (b, c) = (at(&p, "B"), at(&p, "C"));
        assert!(red.3 > (c.1 - b.1) + 10.0, "red spans both rows: h={} rows apart by {}", red.3, c.1 - b.1);
        // a tall spanning cell makes the table as tall as the cell needs
        let short = render("<table><tr><td rowspan=2>one</td><td>B</td></tr><tr><td>C</td></tr></table>");
        let tall = render("<table><tr><td rowspan=2>1<br>2<br>3<br>4<br>5<br>6</td><td>B</td></tr><tr><td>C</td></tr></table>");
        assert!(tall.layout.height > short.layout.height + 60.0, "{} vs {}", tall.layout.height, short.layout.height);
        // and the cells of the last spanned row sit within it
        let (tb, tc) = (at(&tall, "B"), at(&tall, "C"));
        assert!(tc.1 > tb.1);
    }

    #[test]
    fn percentage_columns_fill_or_scale_to_the_table() {
        // under 100% with no auto column: scaled up to fill the 300px table
        let p = render("<table width=300 style='background:red;border-spacing:0'><tr><td width=25%>a</td><td width=25%>b</td></tr></table>");
        let t = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
        assert!((t.2 - 300.0).abs() < 1.0, "table fills 300px: {}", t.2);
        // over 100%: scaled down instead of overflowing
        let p = render("<table width=300 style='background:red;border-spacing:0'><tr><td width=70%>a</td><td width=70%>b</td></tr></table>");
        let t = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
        assert!(t.2 <= 301.0, "no overflow past 300px: {}", t.2);
        let (a, b) = (at(&p, "a"), at(&p, "b"));
        assert!((b.0 - a.0 - 150.0).abs() < 8.0, "two equal halves: {a:?} {b:?}");
        // distribute() directly
        let w = distribute(&[0.0, 0.0], &[10.0, 10.0], &[Some(25.0), Some(25.0)], 300.0);
        assert!((w.iter().sum::<f32>() - 300.0).abs() < 0.01);
        let w = distribute(&[0.0, 0.0], &[10.0, 10.0], &[Some(70.0), Some(70.0)], 300.0);
        assert!((w.iter().sum::<f32>() - 300.0).abs() < 0.01);
    }

    #[test]
    fn captions_widen_the_table_and_collapsed_borders_are_not_doubled() {
        // a caption's longest word is a lower bound on the table width
        let p = render("<table style='background:red'><caption>averyveryverylongcaptionwordthatexceedsthetable</caption><tr><td>a</td></tr></table>");
        let table = rects(&p).into_iter().find(|r| r.4 == RED).unwrap();
        let cap_x = at(&p, "averyveryverylongcaptionwordthatexceedsthetable").0;
        assert!(table.2 > 150.0, "table {} wide", table.2);
        assert!(cap_x >= table.0 - 0.5 && cap_x < table.0 + table.2, "caption starts inside the table");
        // collapsed borders: every drawn border line is the declared 2px total, not 4px between cells
        let p = render("<style>table{border-collapse:collapse} td{border:2px solid red;padding:2px}</style><table><tr><td>a</td><td>b</td></tr><tr><td>c</td><td>d</td></tr></table>");
        let thickest = rects(&p).into_iter().filter(|r| r.4 == RED).map(|r| r.2.min(r.3)).fold(0.0f32, f32::max);
        assert!(thickest <= 1.01, "each cell draws half of a 2px edge, got {thickest}");
    }

    // ---------------------------------------------------------------- form controls

    #[test]
    fn a_text_input_is_an_inline_box_sitting_on_the_text_baseline() {
        let p = render("<p style='margin:0'>name <input name=q> end</p>");
        assert_eq!(p.layout.controls.len(), 1);
        let c = &p.layout.controls[0];
        let (name, end) = (at(&p, "name"), at(&p, "end"));
        assert_eq!(name.1, end.1, "text on both sides shares a baseline");
        assert!(c.y < name.1 && name.1 < c.y + c.h, "the baseline runs through the box: {} not in {}..{}", name.1, c.y, c.y + c.h);
        assert!(c.x > name.0 + 20.0 && end.0 > c.x + c.w, "the box sits between the words");
        assert!(c.w > 100.0 && c.h > 14.0, "a default input is about 20 characters wide");
        // the box is drawn: a grey border rect with a white fill inside it
        let r = rects(&p);
        assert!(r.iter().any(|r| r.4 == CTL_BORDER && (r.2 - c.w).abs() < 0.01));
        assert!(r.iter().any(|r| r.4 == Color::WHITE && r.2 < c.w));
    }

    #[test]
    fn values_placeholders_passwords_and_hidden_inputs() {
        let p = render("<input value='hello'>");
        assert!(texts(&p).iter().any(|(t, _)| t == "hello"));
        // password fields never draw the real text
        let p = render("<input type=password value=secret>");
        let t = texts(&p);
        assert!(t.iter().any(|(s, _)| s == "••••••") && !t.iter().any(|(s, _)| s.contains("secret")), "{t:?}");
        // a placeholder shows while empty, in grey
        let p = render("<input placeholder='Search…'>");
        assert!(texts(&p).iter().any(|(t, _)| t == "Search…"));
        let p = render("<input placeholder='Search…' value='x'>");
        assert!(!texts(&p).iter().any(|(t, _)| t == "Search…"));
        // hidden inputs take no space and have no region
        let p = render("<p style='margin:0'>a<input type=hidden name=h value=1>b</p>");
        assert!(p.layout.controls.is_empty());
        assert_eq!(at(&p, "a").1, at(&p, "b").1);
    }

    #[test]
    fn buttons_checkboxes_radios_and_selects_render() {
        let p = render("<input type=submit><input type=submit value='Go now'><button>Click  me</button><input type=button value=B><input type=reset>");
        let labels: Vec<String> = texts(&p).into_iter().map(|(t, _)| t).collect();
        for want in ["Submit", "Go", "now", "Click", "me", "B", "Reset"] {
            // multi-word labels are one Text item each; accept either form
            assert!(labels.iter().any(|l| l == want || l.contains(want)), "{want} in {labels:?}");
        }
        assert_eq!(p.layout.controls.len(), 5);
        // boxes: a checkbox is 13px square, and a checked one has an inner mark
        let off = render("<input type=checkbox>");
        let on = render("<input type=checkbox checked>");
        let c = &on.layout.controls[0];
        assert_eq!((c.w, c.h), (13.0, 13.0));
        assert!(rects(&on).len() > rects(&off).len(), "the checked box draws an extra mark");
        // a select shows the chosen option and sizes to the widest
        let p = render("<select name=s><option>Short<option selected>A much longer option</select>");
        let t = texts(&p);
        assert!(t.iter().any(|(s, _)| s == "A much longer option") && !t.iter().any(|(s, _)| s == "Short"));
        assert!(p.layout.controls[0].w > 100.0);
    }

    #[test]
    fn focus_draws_a_ring_and_a_caret_and_typing_changes_what_is_shown() {
        let fonts = Fonts::load();
        let mut p = render("<form><input name=q></form>");
        let id = p.layout.controls[0].node;
        let plain = rects(&p).len();
        assert!(!rects(&p).iter().any(|r| r.4 == CTL_FOCUS));
        p.form.focus = Some(id);
        p.relayout(&fonts, 400.0);
        assert!(rects(&p).iter().any(|r| r.4 == CTL_FOCUS), "blue focus border");
        assert_eq!(rects(&p).len(), plain + 1, "plus a caret");
        let (dom, form) = (&p.dom, &mut p.form);
        form.type_text(dom, id, "typed");
        p.relayout(&fonts, 400.0);
        assert!(texts(&p).iter().any(|(t, _)| t == "typed"));
        // the region doesn't move or resize while typing
        assert_eq!(p.layout.controls.len(), 1);
    }

    #[test]
    fn long_values_are_clipped_to_the_box() {
        let fonts = Fonts::load();
        let mut p = render(&format!("<input size=8 value='{}'>", "w".repeat(200)));
        let c = (p.layout.controls[0].x, p.layout.controls[0].w);
        let shown = |p: &Page| texts_full(p).into_iter().find(|(t, _, _)| t.starts_with('w')).unwrap();
        let (t, x, _) = shown(&p);
        assert!(t.len() < 200 && x + fonts.measure(ctl_face(), CTL_FONT, &t) <= c.0 + c.1, "head of the value fits: {t:?}");
        // while editing, the END of the value is what shows, so the caret stays visible
        let id = p.layout.controls[0].node;
        p.dom_value_edit(id, "END");
        p.form.focus = Some(id);
        p.relayout(&fonts, 400.0);
        let (t, ..) = shown_end(&p);
        assert!(t.ends_with("END"), "{t:?}");
    }

    fn shown_end(p: &Page) -> (String, f32, f32) {
        texts_full(p).into_iter().find(|(t, _, _)| t.ends_with("END")).expect("the tail of the value is drawn")
    }

    #[test]
    fn textareas_show_their_last_rows_and_scale_with_rows_and_cols() {
        let small = render("<textarea rows=2 cols=10></textarea>");
        let big = render("<textarea rows=6 cols=40></textarea>");
        assert!(big.layout.controls[0].h > small.layout.controls[0].h * 2.5);
        assert!(big.layout.controls[0].w > small.layout.controls[0].w * 3.0);
        let p = render("<textarea rows=2>one\ntwo\nthree</textarea>");
        let t: Vec<String> = texts(&p).into_iter().map(|(t, _)| t).collect();
        assert_eq!(t, ["two", "three"], "only the last two rows fit");
    }

    #[test]
    fn controls_wrap_flow_in_tables_and_have_clickable_regions() {
        // wraps to the next line in a narrow container, like any inline box
        let p = render_w("<input size=10><input size=10><input size=10>", 200.0);
        assert!(p.layout.controls.len() == 3 && p.layout.controls[2].y > p.layout.controls[0].y + 5.0);
        // inside table cells they move with vertical alignment
        let p = render("<table><tr><td><input name=a></td><td>one<br>two<br>three</td></tr></table>");
        let c = &p.layout.controls[0];
        let top = at(&p, "one").1;
        assert!(c.y + c.h / 2.0 > top - 12.0 && c.y > 0.0, "the control was shifted with its cell: y={} first line {}", c.y, top);
        // every visible control has a region whose rect is inside the document
        let p = render("<form><input name=a><input type=checkbox><button>b</button><select><option>x</select><textarea></textarea></form>");
        assert_eq!(p.layout.controls.len(), 5);
        assert!(p.layout.controls.iter().all(|c| c.w > 0.0 && c.h > 0.0 && c.y + c.h <= p.layout.height + 1.0));
    }

    #[test]
    fn hostile_control_attributes_do_not_break_layout() {
        for body in [
            "<input size=99999 value='x'>", "<input size=-5>", "<input size=abc>", "<textarea rows=99999 cols=99999>x</textarea>",
            "<textarea rows=-3 cols=0></textarea>", "<select></select>", "<select><option></select>", "<button></button>",
            "<input type=submit value=''>", "<input value='é日本語🎉' placeholder='ü'>", "<table><tr><td><input></td></tr></table>",
            "<input style='display:none'>", "<form><form><input></form></form>",
        ] {
            let p = render(body);
            assert!(p.layout.height.is_finite(), "{body}");
        }
    }

    #[test]
    fn pages_with_controls_paint_without_panicking() {
        use crate::paint::Painter;
        let fonts = Fonts::load();
        let mut p = render("<form><input name=a value=hello><input type=password value=x><input type=checkbox checked><button>Go</button><select><option>A</select><textarea>t</textarea></form>");
        p.form.focus = Some(p.layout.controls[0].node);
        p.relayout(&fonts, 400.0);
        let (w, h) = (400usize, 120usize);
        for scale in [1.0, 2.0] {
            Painter::default().paint(&mut vec![0; w * h], w, h, &p.layout, &fonts, 0.0, scale);
        }
    }
}
