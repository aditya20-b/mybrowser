//! Stage 2: CSS text -> rules; selector matching; value parsing helpers.
use crate::dom::{Dom, Kind, NodeId};
use std::rc::Rc;

// ---------- values ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const BLACK: Color = Color { r: 0, g: 0, b: 0 };
    pub const WHITE: Color = Color { r: 255, g: 255, b: 255 };
    pub fn to_u32(self) -> u32 {
        (self.r as u32) << 16 | (self.g as u32) << 8 | self.b as u32
    }
}

/// A CSS <color>: `None` = not a valid color; `Some(None)` = fully transparent;
/// `Some(Some(c))` = a color. (Partial alpha is not composited yet: any alpha
/// above zero is treated as opaque.)
pub fn color_value(s: &str) -> Option<Option<Color>> {
    let s = s.trim().to_ascii_lowercase();
    if s == "transparent" {
        return Some(None);
    }
    if let Some(hex) = s.strip_prefix('#') {
        let d: Vec<u8> = hex.chars().map(|c| c.to_digit(16).map(|d| d as u8)).collect::<Option<_>>()?;
        let (c, alpha) = match d.len() {
            3 => (Color { r: d[0] * 17, g: d[1] * 17, b: d[2] * 17 }, 255),
            4 => (Color { r: d[0] * 17, g: d[1] * 17, b: d[2] * 17 }, d[3] * 17),
            6 => (Color { r: d[0] * 16 + d[1], g: d[2] * 16 + d[3], b: d[4] * 16 + d[5] }, 255),
            8 => (Color { r: d[0] * 16 + d[1], g: d[2] * 16 + d[3], b: d[4] * 16 + d[5] }, d[6] * 16 + d[7]),
            _ => return None,
        };
        return Some((alpha > 0).then_some(c));
    }
    if s.starts_with("rgb") || s.starts_with("hsl") {
        let (open, close) = (s.find('(')?, s.rfind(')')?);
        if close <= open {
            return None;
        }
        let nums: Vec<&str> = s[open + 1..close]
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .collect();
        if nums.len() < 3 {
            return None;
        }
        let num = |t: &str| t.trim_end_matches(|c| c == '%' || c == 'd' || c == 'e' || c == 'g').parse::<f32>().ok();
        let alpha = match nums.get(3) {
            Some(a) => {
                let v = num(a)?;
                if a.ends_with('%') { v / 100.0 } else { v }
            }
            None => 1.0,
        };
        let c = if s.starts_with("rgb") {
            let ch = |t: &str| -> Option<u8> {
                let v = num(t)?;
                Some((if t.ends_with('%') { v * 2.55 } else { v }).clamp(0.0, 255.0).round() as u8)
            };
            Color { r: ch(nums[0])?, g: ch(nums[1])?, b: ch(nums[2])? }
        } else {
            let (h, sat, l) = (num(nums[0])?, num(nums[1])? / 100.0, num(nums[2])? / 100.0);
            hsl_to_rgb(h, sat.clamp(0.0, 1.0), l.clamp(0.0, 1.0))
        };
        return Some((alpha > 0.0).then_some(c));
    }
    let (r, g, b) = match s.as_str() {
        "black" => (0, 0, 0), "white" => (255, 255, 255), "red" => (255, 0, 0),
        "green" => (0, 128, 0), "blue" => (0, 0, 255), "yellow" => (255, 255, 0),
        "gray" | "grey" => (128, 128, 128), "orange" => (255, 165, 0),
        "purple" => (128, 0, 128), "pink" => (255, 192, 203), "brown" => (165, 42, 42),
        "silver" => (192, 192, 192), "navy" => (0, 0, 128), "teal" => (0, 128, 128),
        "maroon" => (128, 0, 0), "lime" => (0, 255, 0), "aqua" | "cyan" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255), "olive" => (128, 128, 0),
        "lightgray" | "lightgrey" => (211, 211, 211), "darkgray" | "darkgrey" => (169, 169, 169),
        "gold" => (255, 215, 0), "tomato" => (255, 99, 71), "coral" => (255, 127, 80),
        "crimson" => (220, 20, 60), "salmon" => (250, 128, 114), "khaki" => (240, 230, 140),
        "indigo" => (75, 0, 130), "violet" => (238, 130, 238), "beige" => (245, 245, 220),
        "ivory" => (255, 255, 240), "lavender" => (230, 230, 250),
        "whitesmoke" => (245, 245, 245), "steelblue" => (70, 130, 180),
        "skyblue" => (135, 206, 235), "royalblue" => (65, 105, 225),
        "darkblue" => (0, 0, 139), "darkgreen" => (0, 100, 0), "darkred" => (139, 0, 0),
        _ => return None,
    };
    Some(Some(Color { r, g, b }))
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> Color {
    let h = h.rem_euclid(360.0) / 360.0;
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f32| {
        t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 { p + (q - p) * 6.0 * t }
            else if t < 0.5 { q }
            else if t < 2.0 / 3.0 { p + (q - p) * (2.0 / 3.0 - t) * 6.0 }
            else { p };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    Color { r: f(h + 1.0 / 3.0), g: f(h), b: f(h - 1.0 / 3.0) }
}

/// An opaque color, or `None` if the value is invalid *or* transparent.
pub fn parse_color(s: &str) -> Option<Color> {
    color_value(s).flatten()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Length {
    Px(f32),
    Pct(f32),
    Auto,
}

pub fn parse_length(s: &str, font_size: f32) -> Option<Length> {
    let s = s.trim().to_ascii_lowercase();
    if s == "auto" {
        return Some(Length::Auto);
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f32 = num.parse().ok()?;
    Some(match unit {
        "" | "px" => Length::Px(n),
        "em" => Length::Px(n * font_size),
        "rem" => Length::Px(n * 16.0),
        "pt" => Length::Px(n * 4.0 / 3.0),
        "ex" => Length::Px(n * font_size / 2.0),
        "%" => Length::Pct(n),
        _ => return None,
    })
}

// ---------- declarations ----------------------------------------------------

#[derive(Clone, Debug)]
pub struct Declaration {
    pub name: String,
    pub value: String,
    pub important: bool,
}

const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];

/// Split a value on whitespace, keeping `rgb(...)` style functions intact.
fn tokens(value: &str) -> Vec<String> {
    let mut out = vec![];
    let (mut cur, mut depth) = (String::new(), 0);
    for c in value.chars() {
        match c {
            '(' => { depth += 1; cur.push(c) }
            ')' => { depth = (depth - 1).max(0); cur.push(c) }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 1-4 value box shorthand -> [top, right, bottom, left].
fn box_values(value: &str) -> Option<[String; 4]> {
    let t = tokens(value);
    let v = match t.len() {
        1 => [&t[0], &t[0], &t[0], &t[0]],
        2 => [&t[0], &t[1], &t[0], &t[1]],
        3 => [&t[0], &t[1], &t[2], &t[1]],
        4 => [&t[0], &t[1], &t[2], &t[3]],
        _ => return None,
    };
    Some(v.map(|s| s.clone()))
}

pub fn border_width_keyword(t: &str) -> Option<&'static str> {
    match t.to_ascii_lowercase().as_str() {
        "thin" => Some("1px"),
        "medium" => Some("3px"),
        "thick" => Some("5px"),
        _ => None,
    }
}

fn is_border_style(t: &str) -> bool {
    matches!(t, "none" | "hidden" | "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset" | "outset")
}

const FONT_SIZE_WORDS: &[&str] =
    &["xx-small", "x-small", "small", "medium", "large", "x-large", "xx-large", "smaller", "larger"];

/// `font: [style] [weight] size[/line-height] family...` -> longhands.
fn expand_font(value: &str) -> Vec<(String, String)> {
    let toks = tokens(value);
    let size_at = toks.iter().position(|t| {
        let base = t.split('/').next().unwrap_or(t).to_ascii_lowercase();
        FONT_SIZE_WORDS.contains(&base.as_str())
            || (base.ends_with(|c: char| c.is_ascii_alphabetic() || c == '%') && parse_length(&base, 16.0).is_some())
    });
    let Some(i) = size_at else { return vec![] };
    let (mut style, mut weight) = ("normal", "normal".to_string());
    for t in &toks[..i] {
        match t.to_ascii_lowercase().as_str() {
            "italic" | "oblique" => style = "italic",
            "bold" | "bolder" | "lighter" => weight = t.to_ascii_lowercase(),
            w if w.parse::<u32>().is_ok() => weight = w.to_string(),
            _ => {}
        }
    }
    let size = toks[i].split('/').next().unwrap_or("").to_string();
    let mut out = vec![
        ("font-style".to_string(), style.to_string()),
        ("font-weight".to_string(), weight),
        ("font-size".to_string(), size),
    ];
    if toks.len() > i + 1 {
        out.push(("font-family".to_string(), toks[i + 1..].join(" ")));
    }
    out
}

/// Expand shorthand properties into longhands.
fn expand(name: &str, value: &str) -> Vec<(String, String)> {
    let sided = |suffix: &str, map: &dyn Fn(String) -> String| -> Vec<(String, String)> {
        match box_values(value) {
            Some(v) => SIDES
                .iter()
                .zip(v)
                .map(|(s, v)| (format!("{}-{s}{suffix}", name.split('-').next().unwrap()), map(v)))
                .collect(),
            None => vec![],
        }
    };
    match name {
        "margin" | "padding" => sided("", &|v| v),
        "border-width" => sided("-width", &|v| border_width_keyword(&v).map(String::from).unwrap_or(v)),
        "border-color" => sided("-color", &|v| v),
        "border-style" => sided("-style", &|v| v),
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: Vec<&str> = match name.strip_prefix("border-") {
                Some(side) => vec![*SIDES.iter().find(|s| **s == side).unwrap()],
                None => SIDES.to_vec(),
            };
            let (mut width, mut color, mut style) = (None, None, None);
            for t in tokens(value) {
                let lt = t.to_ascii_lowercase();
                if is_border_style(&lt) {
                    style = Some(lt);
                } else if let Some(w) = border_width_keyword(&lt) {
                    width = Some(w.to_string());
                } else if parse_length(&t, 16.0).is_some() {
                    width = Some(t);
                } else if color_value(&t).is_some() || lt == "currentcolor" {
                    color = Some(t);
                }
            }
            // The shorthand resets everything it doesn't mention; an omitted
            // style is `none`, which means no border is drawn at all.
            let mut out = vec![];
            for s in sides {
                out.push((format!("border-{s}-width"), width.clone().unwrap_or_else(|| "3px".into())));
                out.push((format!("border-{s}-style"), style.clone().unwrap_or_else(|| "none".into())));
                out.push((format!("border-{s}-color"), color.clone().unwrap_or_else(|| "currentcolor".into())));
            }
            out
        }
        // Always resets the color; images and gradients aren't supported yet.
        "background" => {
            let color = tokens(value).into_iter().find(|t| color_value(t).is_some());
            vec![("background-color".to_string(), color.unwrap_or_else(|| "transparent".into()))]
        }
        "font" => expand_font(value),
        _ => vec![(name.to_string(), value.to_string())],
    }
}

/// Split declarations on `;`, but not inside `(...)` or quoted strings, so
/// `url(data:image/png;base64,AAA)` and `content: ";"` survive.
fn split_declarations(text: &str) -> Vec<&str> {
    let (mut out, mut depth, mut quote, mut start) = (vec![], 0i32, None::<char>, 0);
    for (i, c) in text.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '(' => depth += 1,
                ')' => depth = (depth - 1).max(0),
                ';' if depth == 0 => {
                    out.push(&text[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&text[start..]);
    out
}

pub fn parse_declarations(text: &str) -> Vec<Declaration> {
    let mut out = vec![];
    for chunk in split_declarations(text) {
        let Some((name, value)) = chunk.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim().to_string();
        // `!important`, allowing whitespace around the `!`
        let lower = value.to_ascii_lowercase();
        let important = lower
            .trim_end()
            .strip_suffix("important")
            .is_some_and(|r| r.trim_end().ends_with('!'));
        if important {
            value.truncate(value.rfind('!').unwrap());
            value = value.trim().to_string();
        }
        for (n, v) in expand(&name, &value) {
            out.push(Declaration { name: n, value: v, important });
        }
    }
    out
}

// ---------- selectors -------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Comb {
    Descendant,
    Child,
}

#[derive(Default)]
pub struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
}

pub struct Selector {
    /// (combinator linking to the previous part, compound); parts[0].0 is unused.
    parts: Vec<(Comb, Compound)>,
}

impl Selector {
    pub fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for (_, c) in &self.parts {
            s.0 += c.id.is_some() as u32;
            s.1 += c.classes.len() as u32;
            s.2 += c.tag.is_some() as u32;
        }
        s
    }
}

fn parse_compound(t: &str) -> Option<Compound> {
    let chars: Vec<char> = t.chars().collect();
    let mut i = 0;
    let ident = |i: &mut usize| -> String {
        let start = *i;
        while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '-' || chars[*i] == '_') {
            *i += 1;
        }
        chars[start..*i].iter().collect()
    };
    let mut c = Compound::default();
    if i < chars.len() && chars[i] == '*' {
        i += 1;
    } else if i < chars.len() && chars[i].is_alphabetic() {
        c.tag = Some(ident(&mut i).to_ascii_lowercase());
    }
    while i < chars.len() {
        let kind = chars[i];
        i += 1;
        let name = ident(&mut i);
        if name.is_empty() {
            return None;
        }
        match kind {
            '.' => c.classes.push(name),
            '#' => c.id = Some(name),
            _ => return None, // pseudo-classes, attribute selectors, ... unsupported
        }
    }
    Some(c)
}

pub fn parse_selector(text: &str) -> Option<Selector> {
    let mut toks: Vec<String> = vec![];
    let mut cur = String::new();
    for c in text.trim().chars() {
        if c == '>' || c.is_whitespace() {
            if !cur.is_empty() {
                toks.push(std::mem::take(&mut cur));
            }
            if c == '>' {
                toks.push(">".into());
            }
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    let (mut parts, mut comb) = (vec![], Comb::Descendant);
    for t in toks {
        if t == ">" {
            // leading `> p` or doubled `p > > q` is invalid
            if parts.is_empty() || comb == Comb::Child {
                return None;
            }
            comb = Comb::Child;
            continue;
        }
        parts.push((comb, parse_compound(&t)?));
        comb = Comb::Descendant;
    }
    // a trailing `p >` is invalid too
    (!parts.is_empty() && comb != Comb::Child).then_some(Selector { parts })
}

fn compound_matches(c: &Compound, dom: &Dom, id: NodeId) -> bool {
    let Kind::Element { tag, attrs } = &dom.nodes[id].kind else { return false };
    if c.tag.as_ref().is_some_and(|t| t != tag) {
        return false;
    }
    if c.id.is_some() && attrs.get("id") != c.id.as_ref() {
        return false;
    }
    let have = attrs.get("class").map(|s| s.as_str()).unwrap_or("");
    c.classes.iter().all(|k| have.split_whitespace().any(|h| h == k))
}

pub fn selector_matches(dom: &Dom, id: NodeId, sel: &Selector) -> bool {
    // Descendant combinators retry every ancestor, which is exponential in the
    // number of combinators on deep trees. Remembering which (node, part) pairs
    // already failed bounds the work at depth x parts. Short selectors can't
    // blow up, so they skip the bookkeeping.
    let mut failed = std::collections::HashSet::new();
    let memo = sel.parts.len() > 2;
    match_from(dom, id, sel, sel.parts.len() - 1, memo, &mut failed)
}

fn match_from(
    dom: &Dom,
    id: NodeId,
    sel: &Selector,
    i: usize,
    memo: bool,
    failed: &mut std::collections::HashSet<(NodeId, usize)>,
) -> bool {
    if memo && failed.contains(&(id, i)) {
        return false;
    }
    let ok = match_inner(dom, id, sel, i, memo, failed);
    if !ok && memo {
        failed.insert((id, i));
    }
    ok
}

fn match_inner(
    dom: &Dom,
    id: NodeId,
    sel: &Selector,
    i: usize,
    memo: bool,
    failed: &mut std::collections::HashSet<(NodeId, usize)>,
) -> bool {
    let (comb, comp) = &sel.parts[i];
    if !compound_matches(comp, dom, id) {
        return false;
    }
    if i == 0 {
        return true;
    }
    let mut p = dom.nodes[id].parent;
    match comb {
        Comb::Child => p.is_some_and(|p| match_from(dom, p, sel, i - 1, memo, failed)),
        Comb::Descendant => {
            while let Some(pid) = p {
                if match_from(dom, pid, sel, i - 1, memo, failed) {
                    return true;
                }
                p = dom.nodes[pid].parent;
            }
            false
        }
    }
}

// ---------- stylesheets -----------------------------------------------------

pub struct Rule {
    pub selector: Selector,
    pub decls: Rc<Vec<Declaration>>,
    pub order: usize,
    /// True for the built-in user-agent stylesheet. Origin matters in the
    /// cascade: author rules beat UA rules whatever their specificity.
    pub ua: bool,
}

/// Index of the `}` closing the block opened at `open`, or `s.len()`. Braces
/// inside quoted strings don't count; `nested` also tracks inner `{ }` (for
/// at-rules like `@media`).
fn block_end(s: &str, open: usize, nested: bool) -> usize {
    let (mut depth, mut quote) = (0, None::<char>);
    for (k, c) in s[open..].char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '{' if nested => depth += 1,
                '}' => {
                    if !nested || depth <= 1 {
                        return open + k;
                    }
                    depth -= 1;
                }
                _ => {}
            },
        }
    }
    s.len()
}

/// Parse an author stylesheet, appending its rules to `rules`.
pub fn parse_stylesheet(text: &str, rules: &mut Vec<Rule>) {
    parse_stylesheet_origin(text, rules, false)
}

/// Parse a stylesheet, appending its rules to `rules` (later = higher order).
pub fn parse_stylesheet_origin(text: &str, rules: &mut Vec<Rule>, ua: bool) {
    // strip /* comments */
    let mut clean = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(s) = rest.find("/*") {
        clean.push_str(&rest[..s]);
        rest = rest[s..].find("*/").map_or("", |e| &rest[s + e + 2..]);
    }
    clean.push_str(rest);
    let text = clean.as_str();

    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let Some(brace) = rest.find('{') else { break };
        if rest.trim_start().starts_with('@') {
            // at-rules (@media, @font-face, @import ...) are skipped wholesale
            let semi = rest.find(';');
            if semi.is_some_and(|s| s < brace) {
                i += semi.unwrap() + 1;
                continue;
            }
            i += (block_end(rest, brace, true) + 1).min(rest.len());
            continue;
        }
        let end = block_end(rest, brace, false);
        let decls = Rc::new(parse_declarations(&rest[brace + 1..end]));
        for sel_text in rest[..brace].split(',') {
            if let Some(selector) = parse_selector(sel_text) {
                let order = rules.len();
                rules.push(Rule { selector, decls: decls.clone(), order, ua });
            }
        }
        i += (end + 1).min(rest.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    #[test]
    fn colors() {
        assert_eq!(parse_color("#f00"), Some(Color { r: 255, g: 0, b: 0 }));
        assert_eq!(parse_color("#336699"), Some(Color { r: 0x33, g: 0x66, b: 0x99 }));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(Color { r: 1, g: 2, b: 3 }));
        assert_eq!(parse_color("RED"), Some(Color { r: 255, g: 0, b: 0 }));
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_color("#12"), None);
        assert_eq!(parse_color("#ggg"), None);
    }

    #[test]
    fn lengths() {
        assert_eq!(parse_length("10px", 16.0), Some(Length::Px(10.0)));
        assert_eq!(parse_length("2em", 10.0), Some(Length::Px(20.0)));
        assert_eq!(parse_length("50%", 10.0), Some(Length::Pct(50.0)));
        assert_eq!(parse_length("auto", 10.0), Some(Length::Auto));
        assert_eq!(parse_length("-5px", 10.0), Some(Length::Px(-5.0)));
        assert_eq!(parse_length("wat", 10.0), None);
        assert_eq!(parse_length("10furlongs", 10.0), None);
    }

    fn names(decls: &[Declaration]) -> Vec<(&str, &str)> {
        decls.iter().map(|d| (d.name.as_str(), d.value.as_str())).collect()
    }

    #[test]
    fn margin_shorthand_expands() {
        let d = parse_declarations("margin: 1px 2px");
        assert_eq!(names(&d), [("margin-top", "1px"), ("margin-right", "2px"), ("margin-bottom", "1px"), ("margin-left", "2px")]);
        let d = parse_declarations("padding: 1px 2px 3px");
        assert_eq!(d[3].value, "2px"); // left mirrors right for 3 values
    }

    #[test]
    fn border_shorthand_expands() {
        let d = parse_declarations("border: 2px solid rgb(1,2,3)");
        assert!(d.iter().any(|d| d.name == "border-left-width" && d.value == "2px"));
        assert!(d.iter().any(|d| d.name == "border-top-color" && d.value == "rgb(1,2,3)"));
        let d = parse_declarations("border: none");
        assert!(d.iter().filter(|d| d.name.ends_with("-style")).all(|d| d.value == "none"));
        // no style given means `none` as well: nothing is drawn
        let d = parse_declarations("border: 2px");
        assert!(d.iter().filter(|d| d.name.ends_with("-style")).all(|d| d.value == "none"));
        // keywords work in border-width too
        let d = parse_declarations("border-width: thin thick");
        assert_eq!((d[0].value.as_str(), d[1].value.as_str()), ("1px", "5px"));
    }

    #[test]
    fn important_flag() {
        let d = parse_declarations("color: red !important");
        assert!(d[0].important);
        assert_eq!(d[0].value, "red");
    }

    #[test]
    fn unsupported_selectors_are_dropped_not_misapplied() {
        for s in ["a:hover", "a[href]", "a + b", "a ~ b", "::before", ".", "#"] {
            assert!(parse_selector(s).is_none(), "{s} should be rejected");
        }
    }

    #[test]
    fn selector_matching_and_specificity() {
        let dom = html::parse("<div id=main class='a b'><p><span class=x>t</span></p></div>");
        let span = dom.nodes.iter().position(|n| matches!(&n.kind, Kind::Element { tag, .. } if tag == "span")).unwrap();
        let m = |s: &str| selector_matches(&dom, span, &parse_selector(s).unwrap());
        assert!(m("span") && m(".x") && m("*") && m("div span") && m("p > span") && m("#main .x"));
        assert!(!m("div > span") && !m(".y") && !m("p.a span") && !m("b span"));
        assert_eq!(parse_selector("#a .b c").unwrap().specificity(), (1, 1, 1));
    }

    #[test]
    fn stylesheet_skips_at_rules_and_survives_garbage() {
        let mut rules = vec![];
        parse_stylesheet("@import url(x.css); @media print { p { color: red } } b { color: blue } } {{ i { color: green }", &mut rules);
        assert!(rules.iter().any(|r| r.selector.specificity() == (0, 0, 1)));
        assert!(!rules.is_empty());
        for s in ["", "{", "}", "a{", "@", "@media", "/* unterminated", "a { color: }"] {
            parse_stylesheet(s, &mut vec![]);
        }
    }

    // ---------- regressions from the adversarial review ----------

    #[test]
    fn color_parsing_never_panics_and_handles_alpha_hsl_percent() {
        for s in ["rgb)(", "rgb(", "rgb()", "rgba(1,2)", "hsl(", ")(", "rgb(,,,)", "#", "rgb(1 2 3 /", "hsl(a,b,c)"] {
            let _ = parse_color(s);
        }
        assert_eq!(color_value("rgba(0,0,0,0)"), Some(None)); // fully transparent
        assert_eq!(color_value("transparent"), Some(None));
        assert_eq!(color_value("#0000"), Some(None));
        assert_eq!(color_value("#ff000080"), Some(Some(Color { r: 255, g: 0, b: 0 })));
        assert_eq!(parse_color("rgb(100%, 0%, 50%)"), Some(Color { r: 255, g: 0, b: 128 }));
        assert_eq!(parse_color("hsl(120, 100%, 50%)"), Some(Color { r: 0, g: 255, b: 0 }));
        assert_eq!(parse_color("hsl(0 0% 100%)"), Some(Color { r: 255, g: 255, b: 255 }));
        assert_eq!(color_value("nonsense"), None);
    }

    #[test]
    fn semicolons_inside_urls_and_strings_do_not_split_declarations() {
        let d = parse_declarations("background: url(data:image/png;base64,AAA) red; color:blue; content: \";\"");
        let names: Vec<&str> = d.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["background-color", "color", "content"]);
        assert_eq!(d[0].value, "red");
    }

    #[test]
    fn important_allows_whitespace_and_case() {
        for v in ["red ! important", "red!important", "red !IMPORTANT", "red !important  "] {
            let d = parse_declarations(&format!("color: {v}"));
            assert!(d[0].important, "{v}");
            assert_eq!(d[0].value, "red", "{v}");
        }
        assert!(!parse_declarations("color: important")[0].important);
    }

    #[test]
    fn dangling_combinators_are_rejected() {
        for s in ["> p", "p >", "p > > q", ">"] {
            assert!(parse_selector(s).is_none(), "{s}");
        }
        assert!(parse_selector("p>q").is_some());
    }

    #[test]
    fn deep_descendant_selectors_do_not_blow_up() {
        // Used to take minutes (exponential backtracking).
        let n = 200;
        let src = format!("{}<span>x</span>{}", "<div>".repeat(n), "</div>".repeat(n));
        let dom = html::parse(&src);
        let span = (0..dom.nodes.len()).find(|&i| dom.tag(i) == Some("span")).unwrap();
        let sel = parse_selector("z div div div div div div span").unwrap();
        let t = std::time::Instant::now();
        assert!(!selector_matches(&dom, span, &sel));
        assert!(t.elapsed().as_secs_f32() < 1.0);
        // and memoisation must not change results with child combinators
        let ok = parse_selector("div > div div > span").unwrap();
        assert!(selector_matches(&dom, span, &ok));
    }

    #[test]
    fn braces_inside_strings_do_not_end_blocks() {
        let mut rules = vec![];
        parse_stylesheet("a::before { content: \"}\"; } p { color: red }", &mut rules);
        assert!(rules.iter().any(|r| r.selector.specificity() == (0, 0, 1) && r.decls.iter().any(|d| d.name == "color")));
        let mut rules = vec![];
        parse_stylesheet("@media x { a { content: \"}\" } } p { color: blue }", &mut rules);
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn font_background_and_border_shorthands() {
        let d = parse_declarations("font: italic bold 14px/1.4 Arial, sans-serif");
        let get = |n: &str| d.iter().find(|d| d.name == n).map(|d| d.value.as_str());
        assert_eq!((get("font-style"), get("font-weight"), get("font-size")), (Some("italic"), Some("bold"), Some("14px")));
        assert_eq!(get("font-family"), Some("Arial, sans-serif"));
        // a bare number before the size is a weight, not the size
        let d = parse_declarations("font: 700 2em serif");
        assert_eq!(d.iter().find(|d| d.name == "font-weight").unwrap().value, "700");
        assert_eq!(d.iter().find(|d| d.name == "font-size").unwrap().value, "2em");
        // `background` always resets the colour
        assert_eq!(parse_declarations("background: none")[0].value, "transparent");
        assert_eq!(parse_declarations("background: url(x.png) no-repeat #fff")[0].value, "#fff");
    }
}
