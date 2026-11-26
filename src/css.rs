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

pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(hex) = s.strip_prefix('#') {
        let d: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<_>>()?;
        return match d.len() {
            3 => Some(Color { r: d[0] * 17, g: d[1] * 17, b: d[2] * 17 }),
            6 => Some(Color { r: d[0] * 16 + d[1], g: d[2] * 16 + d[3], b: d[4] * 16 + d[5] }),
            _ => None,
        };
    }
    if s.starts_with("rgb") {
        let inner = &s[s.find('(')? + 1..s.find(')')?];
        let n: Vec<u8> = inner
            .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
            .filter(|p| !p.is_empty())
            .take(3)
            .filter_map(|p| p.parse::<f32>().ok())
            .map(|v| v.clamp(0.0, 255.0) as u8)
            .collect();
        return (n.len() == 3).then(|| Color { r: n[0], g: n[1], b: n[2] });
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
    Some(Color { r, g, b })
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
            ')' => { depth -= 1; cur.push(c) }
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

/// Expand shorthand properties into longhands.
fn expand(name: &str, value: &str) -> Vec<(String, String)> {
    let sided = |suffix: &str| -> Vec<(String, String)> {
        match box_values(value) {
            Some(v) => SIDES
                .iter()
                .zip(v)
                .map(|(s, v)| (format!("{}-{s}{suffix}", suffix_prefix(name)), v))
                .collect(),
            None => vec![],
        }
    };
    match name {
        "margin" | "padding" => sided(""),
        "border-width" => sided("-width"),
        "border-color" => sided("-color"),
        "border" | "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let sides: Vec<&str> = match name.strip_prefix("border-") {
                Some(side) => vec![SIDES.iter().find(|s| **s == side).unwrap()],
                None => SIDES.to_vec(),
            };
            let (mut width, mut color, mut none) = (None, None, false);
            for t in tokens(value) {
                match t.to_ascii_lowercase().as_str() {
                    "none" | "hidden" => none = true,
                    "solid" | "dashed" | "dotted" | "double" | "groove" | "ridge" | "inset"
                    | "outset" => {}
                    "thin" => width = Some("1px".to_string()),
                    "medium" => width = Some("3px".to_string()),
                    "thick" => width = Some("5px".to_string()),
                    _ if parse_length(&t, 16.0).is_some() => width = Some(t),
                    _ if parse_color(&t).is_some() => color = Some(t),
                    _ => {}
                }
            }
            let mut out = vec![];
            for s in sides {
                let w = if none { "0".to_string() } else { width.clone().unwrap_or("3px".into()) };
                out.push((format!("border-{s}-width"), w));
                if let Some(c) = &color {
                    out.push((format!("border-{s}-color"), c.clone()));
                }
            }
            out
        }
        "background" => tokens(value)
            .into_iter()
            .find(|t| parse_color(t).is_some())
            .map(|t| vec![("background-color".to_string(), t)])
            .unwrap_or_default(),
        _ => vec![(name.to_string(), value.to_string())],
    }
}

/// "margin" -> "margin", "border-width" -> "border" (so we can build "border-top-width").
fn suffix_prefix(name: &str) -> &str {
    name.split('-').next().unwrap()
}

pub fn parse_declarations(text: &str) -> Vec<Declaration> {
    let mut out = vec![];
    for chunk in text.split(';') {
        let Some((name, value)) = chunk.split_once(':') else { continue };
        let name = name.trim().to_ascii_lowercase();
        let mut value = value.trim().to_string();
        let important = value.to_ascii_lowercase().ends_with("!important");
        if important {
            value.truncate(value.len() - "!important".len());
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
            comb = Comb::Child;
            continue;
        }
        parts.push((comb, parse_compound(&t)?));
        comb = Comb::Descendant;
    }
    (!parts.is_empty()).then_some(Selector { parts })
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
    match_from(dom, id, sel, sel.parts.len() - 1)
}

fn match_from(dom: &Dom, id: NodeId, sel: &Selector, i: usize) -> bool {
    let (comb, comp) = &sel.parts[i];
    if !compound_matches(comp, dom, id) {
        return false;
    }
    if i == 0 {
        return true;
    }
    let mut p = dom.nodes[id].parent;
    match comb {
        Comb::Child => p.is_some_and(|p| match_from(dom, p, sel, i - 1)),
        Comb::Descendant => {
            while let Some(pid) = p {
                if match_from(dom, pid, sel, i - 1) {
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
}

/// Parse a stylesheet, appending its rules to `rules` (later = higher order).
pub fn parse_stylesheet(text: &str, rules: &mut Vec<Rule>) {
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
            let (mut depth, mut j) = (0, brace);
            for (k, c) in rest[brace..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            j = brace + k + 1;
                            break;
                        }
                    }
                    _ => {}
                }
                j = brace + k + 1;
            }
            i += j;
            continue;
        }
        let end = rest[brace..].find('}').map_or(rest.len(), |e| brace + e);
        let decls = Rc::new(parse_declarations(&rest[brace + 1..end]));
        for sel_text in rest[..brace].split(',') {
            if let Some(selector) = parse_selector(sel_text) {
                let order = rules.len();
                rules.push(Rule { selector, decls: decls.clone(), order });
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
        assert!(d.iter().all(|d| d.name.ends_with("width") && d.value == "0"));
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
}
