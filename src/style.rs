//! Stage 3: DOM + rules -> computed style for every node (cascade + inheritance).
use crate::css::{self, Color, Declaration, Length, Rule};
use crate::dom::{Dom, Kind};
use crate::fonts::{Face, Family};
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    None,
    Inline,
    Block,
    /// A flex container (see `Style::flex_row` and friends for how it lays its children out).
    Flex,
    ListItem,
    Table,
    TableRow,
    /// `<thead>`, `<tbody>`, `<tfoot>`: transparent wrappers around rows.
    TableRowGroup,
    TableCell,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// Vertical alignment of a table cell's content within its row.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum VAlign {
    Top,
    Middle,
    Bottom,
}

/// `justify-content`: where a flex line's leftover space goes along the main axis.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Justify {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// `align-items`: where flex items sit across the line (stretching boxes isn't done: they
/// keep their own height).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AlignItems {
    Start,
    Center,
    End,
}

#[derive(Clone)]
pub struct Style {
    pub display: Display,
    // inherited
    pub color: Color,
    pub font_size: f32,
    pub bold: bool,
    pub italic: bool,
    pub family: Family,
    pub underline: bool,
    pub align: Align,
    pub pre: bool,
    pub list_none: bool,
    /// `border-spacing` (None = the default, or the table's `cellspacing`).
    pub spacing: Option<f32>,
    /// `border-collapse: collapse` (approximated: no spacing between cells).
    pub collapse: bool,
    // not inherited. Sides are ordered top, right, bottom, left.
    pub background: Option<Color>,
    pub margin: [Length; 4],
    pub padding: [Length; 4],
    pub border_w: [f32; 4],
    /// None = currentcolor, Some(None) = transparent, Some(Some(c)) = a color.
    pub border_c: [Option<Option<Color>>; 4],
    /// Whether each side's border-style is something other than none/hidden.
    pub border_on: [bool; 4],
    /// `text-decoration: underline` declared on this very element (not inherited);
    /// `underline` is the propagated result: own OR any ancestor's.
    pub deco_own: bool,
    pub width: Length,
    pub max_width: Option<f32>,
    pub height: Option<f32>,
    pub border_box: bool, // box-sizing: border-box (width/height include padding+border)
    /// `vertical-align` / `valign=`; None = unspecified (cells then use their row's, else middle).
    pub valign: Option<VAlign>,
    // flexbox (container properties first, then the item's own)
    /// `flex-direction: row` (the default); `column` makes the container stack like a block.
    pub flex_row: bool,
    pub flex_wrap: bool,
    pub justify: Justify,
    pub align_items: AlignItems,
    /// `column-gap` and `row-gap` (`gap` sets both).
    pub gap_col: f32,
    pub gap_row: f32,
    /// `flex-grow`: this item's share of a line's leftover space.
    pub flex_grow: f32,
    /// `flex-shrink`: how readily this item gives up width when a line is too full (0 = never).
    pub flex_shrink: f32,
    /// `flex-basis` in px (None = auto: the item's own content width).
    pub flex_basis: Option<f32>,
    /// A percentage `flex-basis` (of the container's width); used when `flex_basis` is None.
    pub flex_basis_pct: Option<f32>,
}

impl Style {
    pub fn initial() -> Style {
        Style {
            display: Display::Inline,
            color: Color::BLACK,
            font_size: 16.0,
            bold: false,
            italic: false,
            family: Family::Sans,
            underline: false,
            align: Align::Left,
            pre: false,
            list_none: false,
            spacing: None,
            collapse: false,
            background: None,
            margin: [Length::Px(0.0); 4],
            padding: [Length::Px(0.0); 4],
            border_w: [0.0; 4],
            border_c: [None; 4],
            border_on: [false; 4],
            deco_own: false,
            width: Length::Auto,
            max_width: None,
            height: None,
            border_box: false,
            valign: None,
            flex_row: true,
            flex_wrap: false,
            justify: Justify::Start,
            align_items: AlignItems::Start,
            gap_col: 0.0,
            gap_row: 0.0,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: None,
            flex_basis_pct: None,
        }
    }

    /// A fresh style carrying over only the inherited properties.
    pub fn inherit(parent: &Style) -> Style {
        Style {
            color: parent.color,
            font_size: parent.font_size,
            bold: parent.bold,
            italic: parent.italic,
            family: parent.family,
            underline: parent.underline,
            align: parent.align,
            pre: parent.pre,
            list_none: parent.list_none,
            spacing: parent.spacing,
            collapse: parent.collapse,
            ..Style::initial()
        }
    }

    /// Border widths that are actually drawn: a side with `border-style: none`
    /// (the default) has no border whatever its width.
    pub fn borders(&self) -> [f32; 4] {
        let mut w = self.border_w;
        for i in 0..4 {
            if !self.border_on[i] {
                w[i] = 0.0;
            }
        }
        w
    }

    pub fn face(&self) -> Face {
        Face { family: self.family, bold: self.bold, italic: self.italic }
    }

    /// Does this box lay its children out side by side in a row?
    pub fn is_flex_row(&self) -> bool {
        self.display == Display::Flex && self.flex_row
    }
}

fn side(name: &str) -> Option<usize> {
    ["top", "right", "bottom", "left"].iter().position(|s| *s == name)
}

fn is_inherited(name: &str) -> bool {
    matches!(name, "color" | "font-size" | "font-weight" | "font-style" | "font-family"
        | "text-align" | "white-space" | "list-style" | "list-style-type" | "border-spacing" | "border-collapse")
}

/// Copy one property from `src` to `dst` (used for inherit / initial / unset).
fn copy_prop(dst: &mut Style, src: &Style, name: &str) {
    match name {
        "color" => dst.color = src.color,
        "font-size" => dst.font_size = src.font_size,
        "font-weight" => dst.bold = src.bold,
        "font-style" => dst.italic = src.italic,
        "font-family" => dst.family = src.family,
        "text-decoration" | "text-decoration-line" => dst.deco_own = src.underline,
        "text-align" => dst.align = src.align,
        "white-space" => dst.pre = src.pre,
        "list-style" | "list-style-type" => dst.list_none = src.list_none,
        "display" => dst.display = src.display,
        "background-color" => dst.background = src.background,
        "width" => dst.width = src.width,
        "max-width" => dst.max_width = src.max_width,
        "height" => dst.height = src.height,
        "box-sizing" => dst.border_box = src.border_box,
        "vertical-align" => dst.valign = src.valign,
        "border-spacing" => dst.spacing = src.spacing,
        "border-collapse" => dst.collapse = src.collapse,
        "flex-direction" => dst.flex_row = src.flex_row,
        "flex-wrap" => dst.flex_wrap = src.flex_wrap,
        "justify-content" => dst.justify = src.justify,
        "align-items" => dst.align_items = src.align_items,
        "column-gap" => dst.gap_col = src.gap_col,
        "row-gap" => dst.gap_row = src.gap_row,
        "gap" => (dst.gap_col, dst.gap_row) = (src.gap_col, src.gap_row),
        "flex-grow" => dst.flex_grow = src.flex_grow,
        "flex-basis" => (dst.flex_basis, dst.flex_basis_pct) = (src.flex_basis, src.flex_basis_pct),
        "flex-shrink" => dst.flex_shrink = src.flex_shrink,
        "flex" => {
            (dst.flex_grow, dst.flex_shrink) = (src.flex_grow, src.flex_shrink);
            (dst.flex_basis, dst.flex_basis_pct) = (src.flex_basis, src.flex_basis_pct);
        }
        _ => {
            if let Some(i) = name.strip_prefix("margin-").and_then(side) {
                dst.margin[i] = src.margin[i];
            } else if let Some(i) = name.strip_prefix("padding-").and_then(side) {
                dst.padding[i] = src.padding[i];
            } else if let Some((sd, what)) = name.strip_prefix("border-").and_then(|r| r.split_once('-')) {
                if let Some(i) = side(sd) {
                    match what {
                        "width" => dst.border_w[i] = src.border_w[i],
                        "style" => dst.border_on[i] = src.border_on[i],
                        "color" => dst.border_c[i] = src.border_c[i],
                        _ => {}
                    }
                }
            }
        }
    }
}

fn parse_valign(v: &str) -> Option<VAlign> {
    match v.trim().to_ascii_lowercase().as_str() {
        "top" | "baseline" | "text-top" => Some(VAlign::Top),
        "middle" | "center" => Some(VAlign::Middle),
        "bottom" | "text-bottom" => Some(VAlign::Bottom),
        _ => None,
    }
}

fn family_from_names(list: &str) -> Option<Family> {
    for f in list.split(',') {
        let f = f.trim().trim_matches(|c| c == '"' || c == '\'').to_ascii_lowercase();
        let fam = match f.as_str() {
            "monospace" | "courier" | "courier new" | "menlo" | "monaco" | "consolas" | "sfmono-regular" => Some(Family::Mono),
            "serif" | "georgia" | "times" | "times new roman" | "palatino" => Some(Family::Serif),
            "sans-serif" | "arial" | "helvetica" | "helvetica neue" | "system-ui" | "verdana" | "tahoma"
            | "-apple-system" | "segoe ui" => Some(Family::Sans),
            _ => None,
        };
        if fam.is_some() {
            return fam;
        }
    }
    None
}

/// A flex basis as (px, percent of the container): a length in px, a percentage (`0%` counts
/// as 0px: "no basis"), or neither for `auto` / `content`.
type Basis = (Option<f32>, Option<f32>);

fn parse_basis(v: &str, fs: f32) -> Option<Basis> {
    match v.trim().to_ascii_lowercase().as_str() {
        "auto" | "content" | "max-content" | "fit-content" => Some((None, None)),
        other => match css::parse_length(other, fs)? {
            Length::Px(p) => Some((Some(p.max(0.0)), None)),
            Length::Pct(p) if p == 0.0 => Some((Some(0.0), None)),
            Length::Pct(p) => Some((None, Some(p.clamp(0.0, 10_000.0)))),
            Length::Auto => Some((None, None)),
        },
    }
}

/// A non-negative, finite number (a flex factor).
fn parse_factor(t: &str) -> Option<f32> {
    t.parse::<f32>().ok().filter(|g| g.is_finite() && *g >= 0.0)
}

/// The `flex` shorthand: `none`, `auto`, `initial`, `<grow>`, `<grow> <shrink>`, and an optional basis.
/// Returns (grow, shrink, basis); shrink is 1 unless the value says otherwise.
fn parse_flex(v: &str, fs: f32) -> Option<(f32, f32, Basis)> {
    let lv = v.trim().to_ascii_lowercase();
    match lv.as_str() {
        "none" => return Some((0.0, 0.0, (None, None))),
        "initial" => return Some((0.0, 1.0, (None, None))),
        "auto" => return Some((1.0, 1.0, (None, None))),
        _ => {}
    }
    let mut tokens: Vec<&str> = lv.split_whitespace().collect();
    // the basis may come first (`flex: 100px 1 1`): move it to the end, where the forms below expect it
    if tokens.len() >= 2 && tokens[0].parse::<f32>().is_err() {
        let basis = tokens.remove(0);
        tokens.push(basis);
    }
    match tokens.len() {
        // `flex: 1` means `1 1 0%`; a lone length (`flex: 200px`) is a basis with grow 0
        1 => match tokens[0].parse::<f32>() {
            Ok(_) => Some((parse_factor(tokens[0])?, 1.0, (Some(0.0), None))),
            Err(_) => Some((0.0, 1.0, parse_basis(tokens[0], fs)?)),
        },
        // `grow shrink` or `grow basis`
        2 => {
            let grow = parse_factor(tokens[0])?;
            match tokens[1].parse::<f32>() {
                Ok(_) => Some((grow, parse_factor(tokens[1])?, (Some(0.0), None))),
                Err(_) => Some((grow, 1.0, parse_basis(tokens[1], fs)?)),
            }
        }
        3 => Some((parse_factor(tokens[0])?, parse_factor(tokens[1])?, parse_basis(tokens[2], fs)?)),
        _ => None,
    }
}

fn apply(s: &mut Style, parent: &Style, d: &Declaration) {
    let (name, v) = (d.name.as_str(), d.value.trim());
    let lv = v.to_ascii_lowercase();
    let fs = s.font_size;

    // CSS-wide keywords work for every property.
    match lv.as_str() {
        "inherit" => return copy_prop(s, parent, name),
        "initial" => return copy_prop(s, &Style::initial(), name),
        "unset" => {
            let src = if is_inherited(name) { parent } else { &Style::initial() };
            return copy_prop(s, src, name);
        }
        _ => {}
    }

    if let Some(i) = name.strip_prefix("margin-").and_then(side) {
        if let Some(l) = css::parse_length(v, fs) { s.margin[i] = l; }
        return;
    }
    if let Some(i) = name.strip_prefix("padding-").and_then(side) {
        if let Some(l) = css::parse_length(v, fs) { s.padding[i] = l; }
        return;
    }
    // Per-side border longhands (border-top-width ...). Other `border-*` names,
    // like border-spacing and border-collapse, fall through to the match below.
    if let Some((sd, what)) = name.strip_prefix("border-").and_then(|r| r.split_once('-')) {
        if let Some(i) = side(sd) {
            match what {
                "width" => {
                    let l = css::border_width_keyword(v).and_then(|k| css::parse_length(k, fs))
                        .or_else(|| css::parse_length(v, fs));
                    if let Some(Length::Px(px)) = l { s.border_w[i] = px.max(0.0); }
                }
                "style" => {
                    if matches!(lv.as_str(), "none" | "hidden") {
                        s.border_on[i] = false;
                    } else if matches!(lv.as_str(), "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset") {
                        s.border_on[i] = true; // all styles are drawn as solid for now
                    }
                }
                "color" => {
                    if lv == "currentcolor" {
                        s.border_c[i] = None;
                    } else if let Some(c) = css::color_value(v) {
                        s.border_c[i] = Some(c);
                    } // an invalid value is ignored, not a reset
                }
                _ => {}
            }
            return;
        }
    }

    match name {
        "display" => match lv.as_str() {
            "none" | "table-column" | "table-column-group" => s.display = Display::None,
            "list-item" => s.display = Display::ListItem,
            "inline" | "contents" => s.display = Display::Inline,
            l if l.starts_with("inline-") => s.display = Display::Inline,
            "table" => s.display = Display::Table,
            "table-row" => s.display = Display::TableRow,
            "table-row-group" | "table-header-group" | "table-footer-group" => s.display = Display::TableRowGroup,
            "table-cell" => s.display = Display::TableCell,
            "flex" => s.display = Display::Flex,
            "block" | "grid" | "table-caption" | "flow-root" => s.display = Display::Block,
            _ => {} // unknown value: ignore the declaration
        },
        "flex-direction" => match lv.as_str() {
            "row" | "row-reverse" => s.flex_row = true,
            "column" | "column-reverse" => s.flex_row = false,
            _ => {}
        },
        "flex-wrap" => match lv.as_str() {
            "wrap" | "wrap-reverse" => s.flex_wrap = true,
            "nowrap" => s.flex_wrap = false,
            _ => {}
        },
        "flex-flow" => {
            for word in lv.split_whitespace() {
                match word {
                    "row" | "row-reverse" => s.flex_row = true,
                    "column" | "column-reverse" => s.flex_row = false,
                    "wrap" | "wrap-reverse" => s.flex_wrap = true,
                    "nowrap" => s.flex_wrap = false,
                    _ => {}
                }
            }
        }
        "justify-content" => match lv.as_str() {
            "flex-start" | "start" | "normal" | "left" | "stretch" => s.justify = Justify::Start,
            "flex-end" | "end" | "right" => s.justify = Justify::End,
            "center" => s.justify = Justify::Center,
            "space-between" => s.justify = Justify::SpaceBetween,
            "space-around" => s.justify = Justify::SpaceAround,
            "space-evenly" => s.justify = Justify::SpaceEvenly,
            _ => {}
        },
        "align-items" => match lv.as_str() {
            "flex-start" | "start" | "stretch" | "normal" | "baseline" => s.align_items = AlignItems::Start,
            "center" => s.align_items = AlignItems::Center,
            "flex-end" | "end" => s.align_items = AlignItems::End,
            _ => {}
        },
        "gap" | "grid-gap" => {
            // `gap: <row> <column>` (one value sets both)
            let mut parts = lv.split_whitespace().map(|p| css::parse_length(p, fs));
            if let Some(Some(Length::Px(row))) = parts.next() {
                let col = match parts.next() {
                    Some(Some(Length::Px(c))) => c,
                    _ => row,
                };
                (s.gap_row, s.gap_col) = (row.clamp(0.0, 10_000.0), col.clamp(0.0, 10_000.0));
            }
        }
        "column-gap" => { if let Some(Length::Px(p)) = css::parse_length(v, fs) { s.gap_col = p.clamp(0.0, 10_000.0); } }
        "row-gap" => { if let Some(Length::Px(p)) = css::parse_length(v, fs) { s.gap_row = p.clamp(0.0, 10_000.0); } }
        "flex-grow" => { if let Some(g) = parse_factor(&lv) { s.flex_grow = g.min(1000.0); } }
        "flex-shrink" => { if let Some(g) = parse_factor(&lv) { s.flex_shrink = g.min(1000.0); } }
        "flex-basis" => { if let Some(b) = parse_basis(v, fs) { (s.flex_basis, s.flex_basis_pct) = b; } }
        "flex" => {
            if let Some((grow, shrink, basis)) = parse_flex(v, fs) {
                (s.flex_grow, s.flex_shrink) = (grow.min(1000.0), shrink.min(1000.0));
                (s.flex_basis, s.flex_basis_pct) = basis;
            }
        }
        "color" => { if let Some(Some(c)) = css::color_value(v) { s.color = c; } }
        "background-color" => { if let Some(c) = css::color_value(v) { s.background = c; } }
        "font-size" => {
            let px = match lv.as_str() {
                "xx-small" => Some(9.0), "x-small" => Some(10.0), "small" => Some(13.0),
                "medium" => Some(16.0), "large" => Some(18.0), "x-large" => Some(24.0),
                "xx-large" => Some(32.0),
                "smaller" => Some(parent.font_size / 1.2),
                "larger" => Some(parent.font_size * 1.2),
                _ => match css::parse_length(v, parent.font_size) {
                    Some(Length::Px(p)) => Some(p),
                    Some(Length::Pct(p)) => Some(parent.font_size * p / 100.0),
                    _ => None,
                },
            };
            // Clamp: an absurd size would ask the rasterizer for a gigantic bitmap.
            if let Some(p) = px { s.font_size = p.clamp(1.0, 1000.0); }
        }
        "font-weight" => match lv.as_str() {
            "bold" | "bolder" => s.bold = true,
            "normal" | "lighter" => s.bold = false,
            n => { if let Ok(n) = n.parse::<u32>() { s.bold = n >= 600; } }
        },
        "font-style" => match lv.as_str() {
            "italic" | "oblique" => s.italic = true,
            "normal" => s.italic = false,
            _ => {}
        },
        "font-family" => {
            if let Some(fam) = family_from_names(&lv) {
                s.family = fam;
            }
        }
        "text-decoration" | "text-decoration-line" => {
            s.deco_own = lv.contains("underline");
        }
        "text-align" => match lv.as_str() {
            "center" => s.align = Align::Center,
            "right" | "end" => s.align = Align::Right,
            "left" | "start" => s.align = Align::Left,
            _ => {}
        },
        "vertical-align" => {
            if let Some(va) = parse_valign(&lv) {
                s.valign = Some(va);
            }
        }
        "border-spacing" => {
            // `border-spacing: 4px 2px` -> use the horizontal value
            let first = lv.split_whitespace().next().unwrap_or("");
            if let Some(Length::Px(px)) = css::parse_length(first, fs) {
                s.spacing = Some(px.clamp(0.0, 100.0));
            }
        }
        "border-collapse" => match lv.as_str() {
            "collapse" => s.collapse = true,
            "separate" => s.collapse = false,
            _ => {}
        },
        "white-space" => s.pre = matches!(lv.as_str(), "pre" | "pre-wrap" | "pre-line"),
        "width" => { if let Some(l) = css::parse_length(v, fs) { s.width = l; } }
        // An unparseable value (calc(), var(), ...) must not wipe an earlier fallback.
        "max-width" => match (lv.as_str(), css::parse_length(v, fs)) {
            ("none", _) => s.max_width = None,
            (_, Some(Length::Px(p))) => s.max_width = Some(p),
            _ => {}
        },
        "height" => match (lv.as_str(), css::parse_length(v, fs)) {
            ("auto", _) => s.height = None,
            (_, Some(Length::Px(p))) => s.height = Some(p),
            _ => {}
        },
        "box-sizing" => s.border_box = lv == "border-box",
        "list-style" | "list-style-type" => s.list_none = lv.contains("none"),
        _ => {}
    }
}

/// A legacy `width="50"` / `width="50%"` attribute value.
fn attr_length(v: &str) -> Option<Length> {
    let v = v.trim();
    let digits: String = v.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let n: f32 = digits.parse().ok()?;
    if n < 0.0 {
        return None;
    }
    Some(if v[digits.len()..].trim_start().starts_with('%') { Length::Pct(n) } else { Length::Px(n) })
}

/// Presentational HTML attributes (`bgcolor`, `align`, `<font color>` ...). They
/// act like very low-priority author rules: applied before the cascade, so any
/// CSS overrides them. Table-based pages from before CSS depend on these.
fn presentational_hints(s: &mut Style, tag: &str, attrs: &HashMap<String, String>) {
    if matches!(tag, "body" | "table" | "tr" | "td" | "th" | "thead" | "tbody" | "tfoot") {
        if let Some(c) = attrs.get("bgcolor").and_then(|v| css::parse_color(v)) {
            s.background = Some(c);
        }
    }
    if matches!(tag, "td" | "th" | "tr" | "thead" | "tbody" | "tfoot" | "caption" | "div" | "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
        match attrs.get("align").map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("center") | Some("middle") => s.align = Align::Center,
            Some("right") => s.align = Align::Right,
            Some("left") => s.align = Align::Left,
            _ => {}
        }
    }
    if matches!(tag, "td" | "th" | "tr" | "thead" | "tbody" | "tfoot") {
        if let Some(va) = attrs.get("valign").and_then(|v| parse_valign(v)) {
            s.valign = Some(va);
        }
    }
    if matches!(tag, "table" | "td" | "th") {
        if let Some(l) = attrs.get("width").and_then(|v| attr_length(v)) {
            s.width = l;
        }
    }
    if tag == "font" {
        if let Some(c) = attrs.get("color").and_then(|v| css::parse_color(v)) {
            s.color = c;
        }
        if let Some(size) = attrs.get("size") {
            let size = size.trim();
            // 1..7 absolute, or +n / -n relative to the default size 3
            let step: Option<i32> = match size.chars().next() {
                Some('+') | Some('-') => size.parse::<i32>().ok().map(|d| 3 + d),
                _ => size.parse::<i32>().ok(),
            };
            if let Some(step) = step {
                s.font_size = [10.0, 13.0, 16.0, 18.0, 24.0, 32.0, 48.0][(step.clamp(1, 7) - 1) as usize];
            }
        }
        if let Some(fam) = attrs.get("face").and_then(|v| family_from_names(v)) {
            s.family = fam;
        }
    }
}

/// Cascade sort key: (important, origin rank, specificity, source order).
/// Normal declarations: UA < author. `!important` flips it: author < UA.
type CascadeKey = (bool, u8, (u32, u32, u32), usize);

/// Compute a style for every node. Because parents always precede their
/// children in the arena, one forward pass is enough.
pub fn compute(dom: &Dom, rules: &[Rule]) -> Vec<Style> {
    let mut styles: Vec<Style> = Vec::with_capacity(dom.nodes.len());
    let default = Style::initial();

    for id in 0..dom.nodes.len() {
        let parent_style = match dom.nodes[id].parent {
            Some(p) => &styles[p],
            None => &default,
        };
        let mut s = Style::inherit(parent_style);

        if let Kind::Element { tag, attrs } = &dom.nodes[id].kind {
            presentational_hints(&mut s, tag, attrs);

            let rank = |important: bool, ua: bool| if important { ua as u8 } else { !ua as u8 };
            let mut matched: Vec<(CascadeKey, &Declaration)> = vec![];
            for rule in rules {
                if css::selector_matches(dom, id, &rule.selector) {
                    for d in rule.decls.iter() {
                        matched.push(((d.important, rank(d.important, rule.ua), rule.selector.specificity(), rule.order), d));
                    }
                }
            }
            let inline = attrs.get("style").map(|t| css::parse_declarations(t)).unwrap_or_default();
            for d in &inline {
                matched.push(((d.important, rank(d.important, false), (1000, 0, 0), usize::MAX), d));
            }
            matched.sort_by_key(|(k, _)| *k);

            // font-size first, so em units in other properties see the final size
            for (_, d) in matched.iter().filter(|(_, d)| d.name == "font-size") {
                apply(&mut s, parent_style, d);
            }
            for (_, d) in matched.iter().filter(|(_, d)| d.name != "font-size") {
                apply(&mut s, parent_style, d);
            }
            // Decorations propagate to descendants instead of being inherited, so a
            // child can't switch off an ancestor's underline with `text-decoration: none`.
            s.underline = parent_style.underline || s.deco_own;
        } else {
            s.display = Display::Inline;
        }
        styles.push(s);
    }
    styles
}

/// The browser's built-in default stylesheet.
pub const USER_AGENT_CSS: &str = "
head, style, script, title, meta, link, template, base, col, colgroup { display: none }
[hidden] { display: none }
html, body, div, section, article, header, footer, nav, main, aside, p, h1, h2, h3, h4, h5,
h6, ul, ol, dl, dt, dd, pre, blockquote, hr, form, figure, figcaption, fieldset, address,
details, summary, center, caption { display: block }
table { display: table; border-spacing: 2px }
thead, tbody, tfoot { display: table-row-group }
tr { display: table-row }
td, th { display: table-cell; padding: 1px }
li { display: list-item }
summary { padding-left: 16px }
body { margin: 8px }
h1 { font-size: 2em; font-weight: bold; margin: 0.67em 0 }
h2 { font-size: 1.5em; font-weight: bold; margin: 0.83em 0 }
h3 { font-size: 1.17em; font-weight: bold; margin: 1em 0 }
h4 { font-size: 1em; font-weight: bold; margin: 1.33em 0 }
h5 { font-size: 0.83em; font-weight: bold; margin: 1.67em 0 }
h6 { font-size: 0.67em; font-weight: bold; margin: 2.33em 0 }
p, ul, ol, dl, pre, figure { margin: 1em 0 }
blockquote { margin: 1em 40px }
ul, ol { padding-left: 40px }
ul ul, ul ol, ol ul, ol ol { margin: 0 }
dd { margin-left: 40px }
center { text-align: center }
caption { text-align: center }
b, strong, th { font-weight: bold }
i, em, cite, var, dfn, address { font-style: italic }
u, ins { text-decoration: underline }
a { color: #0000ee; text-decoration: underline }
code, kbd, samp, tt { font-family: monospace }
pre { font-family: monospace; white-space: pre }
small, sub, sup { font-size: 0.83em }
big { font-size: 1.17em }
mark { background-color: #ffff00 }
hr { margin: 0.5em 0; border-top: 1px solid #999999 }
th { text-align: center }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn style_of(src: &str, tag: &str) -> Style {
        let dom = html::parse(src);
        let mut rules = vec![];
        css::parse_stylesheet_origin(USER_AGENT_CSS, &mut rules, true);
        for id in 0..dom.nodes.len() {
            if dom.tag(id) == Some("style") {
                css::parse_stylesheet(&dom.text_content(id), &mut rules);
            }
        }
        let styles = compute(&dom, &rules);
        let id = (0..dom.nodes.len()).find(|&i| dom.tag(i) == Some(tag)).unwrap();
        styles[id].clone()
    }

    #[test]
    fn specificity_beats_source_order() {
        let s = style_of("<style>#a{color:red} p{color:blue}</style><p id=a>x</p>", "p");
        assert_eq!(s.color, Color { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn inline_style_beats_id_and_important_beats_inline() {
        let s = style_of("<style>#a{color:red}</style><p id=a style='color:blue'>x</p>", "p");
        assert_eq!(s.color, Color { r: 0, g: 0, b: 255 });
        let s = style_of("<style>#a{color:red !important}</style><p id=a style='color:blue'>x</p>", "p");
        assert_eq!(s.color, Color { r: 255, g: 0, b: 0 });
    }

    #[test]
    fn author_css_overrides_user_agent_and_inheritance_works() {
        let s = style_of("<style>body{color:green;font-size:10px}</style><p>x</p>", "p");
        assert_eq!(s.color, Color { r: 0, g: 128, b: 0 });
        assert_eq!(s.font_size, 10.0);
        assert!(!s.bold);
        assert_eq!(s.margin[0], Length::Px(10.0)); // UA `p { margin: 1em 0 }` at 10px
    }

    #[test]
    fn em_in_same_rule_uses_final_font_size_regardless_of_order() {
        let s = style_of("<p style='margin-top: 2em; font-size: 10px'>x</p>", "p");
        assert_eq!(s.margin[0], Length::Px(20.0));
    }

    #[test]
    fn non_inherited_properties_reset() {
        let s = style_of("<style>div{background:red;margin:9px}</style><div><p>x</p></div>", "p");
        assert_eq!(s.background, None);
        assert_eq!(s.margin[1], Length::Px(0.0));
    }

    #[test]
    fn hostile_values_are_clamped_or_ignored() {
        let s = style_of("<p style='font-size:99999999px; color: notacolor; width: wat'>x</p>", "p");
        assert_eq!(s.font_size, 1000.0);
        assert_eq!(s.color, Color::BLACK);
        assert_eq!(s.width, Length::Auto);
    }

    // ---------- flexbox ----------

    #[test]
    fn flex_container_properties_parse() {
        let s = style_of("<div style='display:flex'>x</div>", "div");
        assert_eq!(s.display, Display::Flex);
        assert!(s.is_flex_row() && !s.flex_wrap);
        assert_eq!((s.justify, s.align_items, s.gap_col, s.gap_row), (Justify::Start, AlignItems::Start, 0.0, 0.0));
        let s = style_of(
            "<div style='display:flex;flex-direction:column;flex-wrap:wrap;justify-content:space-between;align-items:center;gap:10px 20px'>x</div>",
            "div",
        );
        assert_eq!(s.display, Display::Flex);
        assert!(!s.flex_row && !s.is_flex_row() && s.flex_wrap);
        assert_eq!((s.justify, s.align_items), (Justify::SpaceBetween, AlignItems::Center));
        assert_eq!((s.gap_row, s.gap_col), (10.0, 20.0));
        let s = style_of("<div style='display:flex;flex-flow:column wrap;column-gap:7px;row-gap:3px;justify-content:flex-end'>x</div>", "div");
        assert!(!s.flex_row && s.flex_wrap);
        assert_eq!((s.gap_col, s.gap_row, s.justify), (7.0, 3.0, Justify::End));
        let one = style_of("<div style='gap:12px'>x</div>", "div");
        assert_eq!((one.gap_col, one.gap_row), (12.0, 12.0), "a single value sets both");
        for (value, want) in [
            ("center", Justify::Center), ("space-around", Justify::SpaceAround), ("space-evenly", Justify::SpaceEvenly),
            ("flex-start", Justify::Start), ("end", Justify::End),
        ] {
            assert_eq!(style_of(&format!("<div style='justify-content:{value}'>x</div>"), "div").justify, want, "{value}");
        }
    }

    #[test]
    fn flex_item_shorthands() {
        let item = |decl: &str| {
            let s = style_of(&format!("<p style='{decl}'>x</p>"), "p");
            (s.flex_grow, s.flex_basis)
        };
        assert_eq!(item("flex: 1"), (1.0, Some(0.0)), "`flex: 1` is `1 1 0%`");
        assert_eq!(item("flex: 3"), (3.0, Some(0.0)));
        assert_eq!(item("flex: 2 1 0%"), (2.0, Some(0.0)));
        assert_eq!(item("flex: 1 1 100px"), (1.0, Some(100.0)));
        assert_eq!(item("flex: 0 0 auto"), (0.0, None));
        assert_eq!(item("flex: auto"), (1.0, None));
        assert_eq!(item("flex: none"), (0.0, None));
        assert_eq!(item("flex: 200px"), (0.0, Some(200.0)), "a lone length is a basis");
        assert_eq!(item("flex: 1 2"), (1.0, Some(0.0)), "two numbers are grow and shrink");
        assert_eq!(item("flex: 1 50px"), (1.0, Some(50.0)), "grow and basis");
        // the basis may be written first
        assert_eq!(item("flex: 100px 2"), (2.0, Some(100.0)));
        assert_eq!(item("flex: 100px 3 1"), (3.0, Some(100.0)));
        assert_eq!(item("flex: auto 1 1"), (1.0, None));
        assert_eq!(item("flex: 100px 100px"), (0.0, None), "two bases make no sense");
        assert_eq!(item("flex: banana 1"), (0.0, None));
        let shrink = |decl: &str| style_of(&format!("<p style='{decl}'>x</p>"), "p").flex_shrink;
        assert_eq!((shrink(""), shrink("flex: 1"), shrink("flex: none"), shrink("flex: auto")), (1.0, 1.0, 0.0, 1.0));
        assert_eq!((shrink("flex: 1 0 auto"), shrink("flex: 2 3"), shrink("flex-shrink: 0")), (0.0, 3.0, 0.0));
        assert_eq!((shrink("flex-shrink: -1"), shrink("flex: 1 -1")), (1.0, 1.0), "nonsense is ignored");
        let pct = |decl: &str| style_of(&format!("<p style='{decl}'>x</p>"), "p").flex_basis_pct;
        assert_eq!((pct("flex-basis: 33%"), pct("flex: 0 0 25%"), pct("flex: 1 50%")), (Some(33.0), Some(25.0), Some(50.0)));
        assert_eq!((pct("flex-basis: 0%"), pct("flex: 1"), pct("flex-basis: 20px"), pct("flex-basis: auto")), (None, None, None, None));
        assert_eq!(pct("flex-basis: 40%; flex-basis: 10px"), None, "a later px basis replaces the percentage");
        assert_eq!(item("flex-grow: 2.5"), (2.5, None));
        assert_eq!(item("flex-basis: 40px"), (0.0, Some(40.0)));
        assert_eq!(item("flex-grow: 2; flex-basis: auto"), (2.0, None));
        // nonsense is ignored, hostile numbers are clamped
        assert_eq!(item("flex: banana"), (0.0, None));
        assert_eq!(item("flex: -3"), (0.0, None));
        assert_eq!(item("flex: 1 -2"), (0.0, None));
        assert_eq!(item("flex: 1 2 3 4"), (0.0, None));
        assert_eq!(item("flex: 99999999").0, 1000.0);
        assert_eq!(item("flex-grow: -1"), (0.0, None));
        // flex props are not inherited
        let s = style_of("<div style='display:flex;gap:9px;flex:2'><p>x</p></div>", "p");
        assert_eq!((s.gap_col, s.flex_grow, s.display), (0.0, 0.0, Display::Block));
    }

    // ---------- regressions from the adversarial review ----------

    #[test]
    fn the_hidden_attribute_hides_an_element_unless_the_page_says_otherwise() {
        assert_eq!(style_of("<div hidden>x</div>", "div").display, Display::None);
        assert_eq!(style_of("<span hidden>x</span>", "span").display, Display::None);
        assert_eq!(style_of("<p hidden=until-found>x</p>", "p").display, Display::None);
        // an author rule beats the user-agent one, as in browsers
        assert_eq!(style_of("<style>div{display:block}</style><div hidden>x</div>", "div").display, Display::Block);
        // elements without the attribute are untouched, and a child of a hidden one hides with it
        assert_eq!(style_of("<div>x</div>", "div").display, Display::Block);
        assert_eq!(style_of("<div hidden><p>x</p></div>", "p").display, Display::Block, "(its box is simply never laid out)");
    }

    #[test]
    fn author_rules_beat_user_agent_rules_regardless_of_specificity() {
        // the classic reset: `*` (specificity 0) must beat UA `p { margin: 1em 0 }`
        let s = style_of("<style>* { margin: 0 }</style><p>x</p>", "p");
        assert_eq!(s.margin, [Length::Px(0.0); 4]);
        let s = style_of("<style>ul { margin: 20px }</style><ul><ul><li>x</li></ul></ul>", "ul");
        assert_eq!(s.margin[0], Length::Px(20.0));
        // UA !important would still beat author normal rules; author !important beats UA !important
        let s = style_of("<style>p { margin-top: 3px !important }</style><p>x</p>", "p");
        assert_eq!(s.margin[0], Length::Px(3.0));
    }

    #[test]
    fn transparent_and_invalid_values() {
        // transparent border draws nothing; it is not currentcolor
        let s = style_of("<p style='border: 2px solid transparent'>x</p>", "p");
        assert_eq!(s.border_c[0], Some(None));
        assert_eq!(s.borders(), [2.0; 4]);
        // invalid declarations are ignored rather than resetting the earlier value
        let s = style_of("<p style='background-color:red; background-color:notacolor; max-width:600px; max-width:calc(1px+2px); height:5px; height:wat'>x</p>", "p");
        assert_eq!(s.background, Some(Color { r: 255, g: 0, b: 0 }));
        assert_eq!((s.max_width, s.height), (Some(600.0), Some(5.0)));
        // but explicit keywords do reset
        let s = style_of("<p style='max-width:600px; max-width:none; background:red; background:none'>x</p>", "p");
        assert_eq!((s.max_width, s.background), (None, None));
    }

    #[test]
    fn border_style_controls_whether_a_border_is_drawn() {
        let s = style_of("<p style='border: 2px'>x</p>", "p");
        assert_eq!(s.borders(), [0.0; 4]); // no style = none
        let s = style_of("<p style='border-width: 2px; border-style: solid'>x</p>", "p");
        assert_eq!(s.borders(), [2.0; 4]);
        let s = style_of("<p style='border: 2px solid red; border-top-style: none'>x</p>", "p");
        assert_eq!(s.borders(), [0.0, 2.0, 2.0, 2.0]);
        let s = style_of("<p style='border-width: thin; border-style: dashed'>x</p>", "p");
        assert_eq!(s.borders(), [1.0; 4]);
        // a later `border: 1px solid` resets the colour too
        let s = style_of("<p style='border: 1px solid red; border: 1px solid'>x</p>", "p");
        assert_eq!(s.border_c[0], None);
    }

    #[test]
    fn font_shorthand_applies() {
        let s = style_of("<p style='font: italic bold 20px/1.5 Georgia, serif'>x</p>", "p");
        assert_eq!((s.font_size, s.bold, s.italic, s.family), (20.0, true, true, Family::Serif));
        let s = style_of("<p style='font-weight:bold; font: 12px Arial'>x</p>", "p");
        assert!(!s.bold, "font shorthand resets weight to normal");
    }

    #[test]
    fn css_wide_keywords() {
        let s = style_of("<style>table{text-align:right} th{text-align:inherit}</style><table><th>x</th></table>", "th");
        assert_eq!(s.align, Align::Right); // UA says center; author `inherit` wins
        let s = style_of("<p style='color:red'><b style='color:inherit; font-weight:initial; margin:inherit'>x</b></p>", "b");
        assert_eq!(s.color, Color { r: 255, g: 0, b: 0 });
        assert!(!s.bold);
        let s = style_of("<div style='margin-left:9px'><p style='margin-left:inherit'>x</p></div>", "p");
        assert_eq!(s.margin[3], Length::Px(9.0));
        let s = style_of("<p style='color:red'><b style='color:unset;margin-top:unset'>x</b></p>", "b");
        assert_eq!(s.color, Color { r: 255, g: 0, b: 0 }); // unset = inherit for inherited props
    }

    #[test]
    fn text_decoration_propagates_and_can_be_removed_on_the_declaring_element() {
        let s = style_of("<u><span style='text-decoration:none'>x</span></u>", "span");
        assert!(s.underline, "a descendant can't remove an ancestor's underline");
        let s = style_of("<style>a{text-decoration:none}</style><a href=x>l</a>", "a");
        assert!(!s.underline, "but the element that declared it can drop its own");
        let s = style_of("<a href=x><b>l</b></a>", "b");
        assert!(s.underline, "links underline their descendants");
    }

    #[test]
    fn display_keywords() {
        let d = |v: &str| style_of(&format!("<p style='display:{v}'>x</p>"), "p").display;
        assert_eq!(d("inline-grid"), Display::Inline);
        assert_eq!(d("inline-flex"), Display::Inline);
        assert_eq!(d("contents"), Display::Inline);
        assert_eq!(d("flex"), Display::Flex);
        assert_eq!(d("grid"), Display::Block);
        assert_eq!(d("bogus"), Display::Block); // ignored: keeps the UA default for <p>
        assert_eq!(style_of("<div style='display:inline'><p style='display:inherit'>x</p></div>", "p").display, Display::Inline);
    }

    // ---------- tables and presentational attributes ----------

    #[test]
    fn table_elements_get_table_display_types() {
        let t = "<table><thead><tr><th>h</th></tr></thead><tbody><tr><td>c</td></tr></tbody><caption>c</caption></table>";
        for (tag, display) in [
            ("table", Display::Table), ("thead", Display::TableRowGroup), ("tbody", Display::TableRowGroup),
            ("tr", Display::TableRow), ("th", Display::TableCell), ("td", Display::TableCell), ("caption", Display::Block),
        ] {
            assert_eq!(style_of(t, tag).display, display, "{tag}");
        }
        // CSS can turn other elements into table parts, and columns are ignored
        assert_eq!(style_of("<div style='display:table-cell'>x</div>", "div").display, Display::TableCell);
        assert_eq!(style_of("<div style='display:table-row-group'>x</div>", "div").display, Display::TableRowGroup);
        assert_eq!(style_of("<div style='display:table-column'>x</div>", "div").display, Display::None);
    }

    #[test]
    fn presentational_attributes_apply_but_css_overrides_them() {
        let s = style_of("<table bgcolor='#ff6600' width='85%'><tr><td align=right valign=top bgcolor=red width=120>x</td></tr></table>", "table");
        assert_eq!((s.background, s.width), (Some(Color { r: 255, g: 102, b: 0 }), Length::Pct(85.0)));
        let s = style_of("<table><tr><td align=right valign=top bgcolor=red width=120>x</td></tr></table>", "td");
        assert_eq!((s.align, s.valign, s.width), (Align::Right, Some(VAlign::Top), Length::Px(120.0)));
        assert_eq!(s.background, Some(Color { r: 255, g: 0, b: 0 }));
        // author CSS beats the attributes
        let s = style_of("<style>td{background:blue;text-align:left;width:50px}</style><table><tr><td align=right bgcolor=red width=120>x</td></tr></table>", "td");
        assert_eq!((s.background, s.align, s.width), (Some(Color { r: 0, g: 0, b: 255 }), Align::Left, Length::Px(50.0)));
    }

    #[test]
    fn font_tag_attributes() {
        let s = style_of("<p><font color='#828282' size='1' face='Verdana, Arial'>x</font></p>", "font");
        assert_eq!((s.color, s.font_size, s.family), (Color { r: 0x82, g: 0x82, b: 0x82 }, 10.0, Family::Sans));
        assert_eq!(style_of("<font size=+2>x</font>", "font").font_size, 24.0);
        assert_eq!(style_of("<font size=-1>x</font>", "font").font_size, 13.0);
        assert_eq!(style_of("<font size=99>x</font>", "font").font_size, 48.0);
        assert_eq!(style_of("<font size=junk>x</font>", "font").font_size, 16.0);
        assert_eq!(style_of("<font face='Courier New'>x</font>", "font").family, Family::Mono);
    }

    #[test]
    fn table_spacing_valign_and_collapse_properties() {
        // border-spacing / border-collapse must not be swallowed by the border-<side> handling
        let s = style_of("<table style='border-spacing: 7px 3px; border-collapse: collapse'><tr><td style='vertical-align: bottom'>x</td></tr></table>", "table");
        assert_eq!((s.spacing, s.collapse), (Some(7.0), true));
        assert_eq!(style_of("<table><tr><td style='vertical-align: bottom'>x</td></tr></table>", "td").valign, Some(VAlign::Bottom));
        // spacing and collapse are inherited by cells, valign is not
        let s = style_of("<table style='border-spacing: 5px'><tr><td>x</td></tr></table>", "td");
        assert_eq!(s.spacing, Some(5.0));
        assert_eq!(style_of("<table><tr valign=top><td>x</td></tr></table>", "td").valign, None);
        // the UA default spacing is 2px, and hostile values are clamped
        assert_eq!(style_of("<table><tr><td>x</td></tr></table>", "table").spacing, Some(2.0));
        assert_eq!(style_of("<table style='border-spacing: 99999px'><tr><td>x</td></tr></table>", "table").spacing, Some(100.0));
        // and the per-side border longhands still work next to them
        let s = style_of("<table style='border-top-width: 3px; border-top-style: solid; border-spacing: 1px'><tr><td>x</td></tr></table>", "table");
        assert_eq!((s.borders()[0], s.spacing), (3.0, Some(1.0)));
    }

    #[test]
    fn legacy_width_attribute_parsing() {
        assert_eq!(attr_length("100"), Some(Length::Px(100.0)));
        assert_eq!(attr_length(" 50% "), Some(Length::Pct(50.0)));
        assert_eq!(attr_length("12.5px"), Some(Length::Px(12.5)));
        assert_eq!(attr_length("auto"), None);
        assert_eq!(attr_length(""), None);
        assert_eq!(attr_length("-5"), None);
    }
}
