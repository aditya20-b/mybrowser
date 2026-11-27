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
//
// Supported: type, `*`, `.class`, `#id`, attribute selectors (`[a]`, `[a=v]`, `~=`,
// `|=`, `^=`, `$=`, `*=`, with an optional `i` flag), the structural pseudo-classes
// (`:first-child`, `:last-child`, `:only-child`, `:nth-child()`, `:nth-last-child()`
// and the `-of-type` versions), `:link`, `:root` and `:not()`, and the descendant,
// child (`>`), adjacent (`+`) and general sibling (`~`) combinators. Anything that
// needs state this browser doesn't have (`:hover`, `:focus`, `:visited`, ...) or
// pseudo-elements (`::before`) makes the whole selector unsupported, so its rule is
// dropped instead of being applied where it doesn't belong.

#[derive(Clone, Copy, PartialEq)]
pub enum Comb {
    Descendant,
    Child,
    /// `a + b`: b directly follows a.
    Adjacent,
    /// `a ~ b`: b follows a somewhere later among the same parent's children.
    Sibling,
}

#[derive(Clone, Copy, PartialEq)]
enum AttrOp {
    Exists,
    Equals,
    /// `~=`: a whitespace-separated word.
    Includes,
    /// `|=`: equal, or followed by `-`.
    DashMatch,
    Prefix,
    Suffix,
    Substring,
}

struct AttrSel {
    name: String,
    op: AttrOp,
    value: String,
    ignore_case: bool,
}

enum Pseudo {
    /// `:nth-child(an+b)` and friends; `:first-child` is `0n+1`.
    Nth { a: i64, b: i64, from_end: bool, of_type: bool },
    Link,
    Root,
    /// `:not(a, b)`: none of the compounds match.
    Not(Vec<Compound>),
}

#[derive(Default)]
pub struct Compound {
    tag: Option<String>,
    id: Option<String>,
    classes: Vec<String>,
    attrs: Vec<AttrSel>,
    pseudos: Vec<Pseudo>,
}

pub struct Selector {
    /// (combinator linking to the previous part, compound); parts[0].0 is unused.
    parts: Vec<(Comb, Compound)>,
}

fn add_specificity(c: &Compound, s: &mut (u32, u32, u32)) {
    s.0 += c.id.is_some() as u32;
    s.1 += (c.classes.len() + c.attrs.len()) as u32;
    s.2 += c.tag.is_some() as u32;
    for p in &c.pseudos {
        match p {
            // `:not()` counts as its most specific argument, not as itself
            Pseudo::Not(args) => {
                let best = args
                    .iter()
                    .map(|a| {
                        let mut t = (0, 0, 0);
                        add_specificity(a, &mut t);
                        t
                    })
                    .max()
                    .unwrap_or((0, 0, 0));
                s.0 += best.0;
                s.1 += best.1;
                s.2 += best.2;
            }
            _ => s.1 += 1,
        }
    }
}

impl Selector {
    pub fn specificity(&self) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for (_, c) in &self.parts {
            add_specificity(c, &mut s);
        }
        s
    }
}

/// The index of the bracket that closes the one at `open` (`[`/`(`), skipping quoted strings.
fn closing(chars: &[char], open: usize) -> Option<usize> {
    let (opener, closer) = if chars[open] == '[' { ('[', ']') } else { ('(', ')') };
    let (mut depth, mut quote) = (0, None::<char>);
    for (k, &c) in chars.iter().enumerate().skip(open) {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == opener => depth += 1,
            None if c == closer => {
                depth -= 1;
                if depth == 0 {
                    return Some(k);
                }
            }
            None => {}
        }
    }
    None
}

/// Split on commas that are not inside brackets, parentheses or quotes.
fn split_top_level_commas(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut quote, mut start) = (vec![], 0i32, None::<char>, 0);
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '[' | '(' => depth += 1,
                ']' | ')' => depth = (depth - 1).max(0),
                ',' if depth == 0 => {
                    out.push(&s[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    out.push(&s[start..]);
    out
}

/// `an+b` as written in `:nth-child(...)`: `odd`, `even`, `3`, `2n+1`, `-n+3`, `n`.
fn parse_nth(arg: &str) -> Option<(i64, i64)> {
    let s: String = arg.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    match s.as_str() {
        "odd" => Some((2, 1)),
        "even" => Some((2, 0)),
        _ => match s.find('n') {
            Some(n) => {
                let a = match &s[..n] {
                    "" | "+" => 1,
                    "-" => -1,
                    digits => digits.parse().ok()?,
                };
                let b = if s[n + 1..].is_empty() { 0 } else { s[n + 1..].parse().ok()? };
                Some((a, b))
            }
            None => Some((0, s.parse().ok()?)),
        },
    }
}

fn parse_attr(inner: &str) -> Option<AttrSel> {
    let inner = inner.trim();
    let name_end = inner.find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || c == ':')).unwrap_or(inner.len());
    let name = inner[..name_end].to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let rest = inner[name_end..].trim_start();
    if rest.is_empty() {
        return Some(AttrSel { name, op: AttrOp::Exists, value: String::new(), ignore_case: false });
    }
    let (op, len) = if rest.starts_with('=') {
        (AttrOp::Equals, 1)
    } else {
        let op = match rest.get(..2)? {
            "~=" => AttrOp::Includes,
            "|=" => AttrOp::DashMatch,
            "^=" => AttrOp::Prefix,
            "$=" => AttrOp::Suffix,
            "*=" => AttrOp::Substring,
            _ => return None,
        };
        (op, 2)
    };
    let rest = rest[len..].trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'');
    let (value, tail) = match quote {
        Some(q) => {
            let body = &rest[1..];
            let end = body.find(q)?;
            (&body[..end], &body[end + 1..])
        }
        None => {
            let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
            if end == 0 {
                return None; // `[a=]`
            }
            (&rest[..end], &rest[end..])
        }
    };
    let ignore_case = match tail.trim().to_ascii_lowercase().as_str() {
        "" | "s" => false,
        "i" => true,
        _ => return None,
    };
    Some(AttrSel { name, op, value: value.to_string(), ignore_case })
}

/// Turn `:name` or `:name(arg)` into pseudo-classes on `c`, or `None` if unsupported.
fn add_pseudo(c: &mut Compound, name: &str, arg: Option<&str>) -> Option<()> {
    let nth = |a, b, from_end, of_type| Pseudo::Nth { a, b, from_end, of_type };
    match (name, arg) {
        ("first-child", None) => c.pseudos.push(nth(0, 1, false, false)),
        ("last-child", None) => c.pseudos.push(nth(0, 1, true, false)),
        ("only-child", None) => c.pseudos.extend([nth(0, 1, false, false), nth(0, 1, true, false)]),
        ("first-of-type", None) => c.pseudos.push(nth(0, 1, false, true)),
        ("last-of-type", None) => c.pseudos.push(nth(0, 1, true, true)),
        ("only-of-type", None) => c.pseudos.extend([nth(0, 1, false, true), nth(0, 1, true, true)]),
        ("nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type", Some(arg)) => {
            let (a, b) = parse_nth(arg)?;
            c.pseudos.push(nth(a, b, name.contains("last"), name.contains("of-type")));
        }
        // every link is "unvisited": the browser keeps no visited-link state
        ("link" | "any-link", None) => c.pseudos.push(Pseudo::Link),
        ("root", None) => c.pseudos.push(Pseudo::Root),
        ("not", Some(arg)) => {
            let args: Option<Vec<Compound>> = split_top_level_commas(arg).into_iter().map(|a| parse_compound(a.trim())).collect();
            let args = args?;
            // `:not()` and `:not(,)` select nothing useful: an empty argument is invalid
            let empty = |a: &Compound| a.tag.is_none() && a.id.is_none() && a.classes.is_empty() && a.attrs.is_empty() && a.pseudos.is_empty();
            if args.is_empty() || args.iter().any(|a| empty(a) && !arg.contains('*')) {
                return None;
            }
            c.pseudos.push(Pseudo::Not(args));
        }
        _ => return None, // :hover, :focus, :visited, ::before, unknown names, bad arguments
    }
    Some(())
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
        match chars[i] {
            '.' | '#' => {
                let kind = chars[i];
                i += 1;
                let name = ident(&mut i);
                if name.is_empty() {
                    return None;
                }
                if kind == '.' {
                    c.classes.push(name);
                } else {
                    c.id = Some(name);
                }
            }
            '[' => {
                let end = closing(&chars, i)?;
                let inner: String = chars[i + 1..end].iter().collect();
                c.attrs.push(parse_attr(&inner)?);
                i = end + 1;
            }
            ':' => {
                i += 1;
                if chars.get(i) == Some(&':') {
                    return None; // pseudo-elements
                }
                let name = ident(&mut i).to_ascii_lowercase();
                if name.is_empty() {
                    return None;
                }
                let arg = if chars.get(i) == Some(&'(') {
                    let end = closing(&chars, i)?;
                    let inner: String = chars[i + 1..end].iter().collect();
                    i = end + 1;
                    Some(inner)
                } else {
                    None
                };
                add_pseudo(&mut c, &name, arg.as_deref())?;
            }
            _ => return None,
        }
    }
    Some(c)
}

pub fn parse_selector(text: &str) -> Option<Selector> {
    // Split into compounds and combinators. Whitespace, `>`, `+` and `~` only
    // separate things outside [...] and (...) and quotes (`:nth-child(2n+1)`, `[a~=b]`).
    let mut toks: Vec<String> = vec![];
    let mut cur = String::new();
    let (mut depth, mut quote) = (0i32, None::<char>);
    for c in text.trim().chars() {
        if let Some(q) = quote {
            cur.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '"' | '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            '[' | '(' => {
                depth += 1;
                cur.push(c);
            }
            ']' | ')' => {
                depth = (depth - 1).max(0);
                cur.push(c);
            }
            c if depth == 0 && (c.is_whitespace() || matches!(c, '>' | '+' | '~')) => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
                if !c.is_whitespace() {
                    toks.push(c.to_string());
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    let (mut parts, mut comb, mut pending) = (vec![], Comb::Descendant, false);
    for t in toks {
        let explicit = match t.as_str() {
            ">" => Some(Comb::Child),
            "+" => Some(Comb::Adjacent),
            "~" => Some(Comb::Sibling),
            _ => None,
        };
        if let Some(e) = explicit {
            // a leading `> p` or a doubled `p > > q` is invalid
            if parts.is_empty() || pending {
                return None;
            }
            comb = e;
            pending = true;
            continue;
        }
        parts.push((comb, parse_compound(&t)?));
        comb = Comb::Descendant;
        pending = false;
    }
    // a trailing `p >` is invalid too
    (!parts.is_empty() && !pending).then_some(Selector { parts })
}

fn is_element(dom: &Dom, id: NodeId) -> bool {
    matches!(dom.nodes[id].kind, Kind::Element { .. })
}

/// The element children of `parent` (text and comments don't count for `:nth-child`).
fn element_children(dom: &Dom, parent: NodeId) -> impl Iterator<Item = NodeId> + '_ {
    dom.nodes[parent].children.iter().copied().filter(|&n| is_element(dom, n))
}

/// Does position `pos` (1-based) fit `an+b` for some n >= 0?
fn nth_fits(a: i64, b: i64, pos: i64) -> bool {
    if a == 0 {
        return pos == b;
    }
    let d = pos - b;
    d % a == 0 && d / a >= 0
}

fn attr_matches(sel: &AttrSel, attrs: &std::collections::HashMap<String, String>) -> bool {
    let Some(have) = attrs.get(&sel.name) else { return false };
    if sel.op == AttrOp::Exists {
        return true;
    }
    let (have, want) = if sel.ignore_case { (have.to_lowercase(), sel.value.to_lowercase()) } else { (have.clone(), sel.value.clone()) };
    match sel.op {
        AttrOp::Exists => true,
        AttrOp::Equals => have == want,
        AttrOp::Includes => !want.is_empty() && !want.contains(char::is_whitespace) && have.split_whitespace().any(|w| w == want),
        AttrOp::DashMatch => have == want || have.strip_prefix(&want).is_some_and(|r| r.starts_with('-')),
        AttrOp::Prefix => !want.is_empty() && have.starts_with(&want),
        AttrOp::Suffix => !want.is_empty() && have.ends_with(&want),
        AttrOp::Substring => !want.is_empty() && have.contains(&want),
    }
}

fn pseudo_matches(p: &Pseudo, dom: &Dom, id: NodeId, tag: &str, attrs: &std::collections::HashMap<String, String>) -> bool {
    match p {
        Pseudo::Nth { a, b, from_end, of_type } => {
            let Some(parent) = dom.nodes[id].parent else { return false };
            let same: Vec<NodeId> = element_children(dom, parent).filter(|&n| !*of_type || dom.tag(n) == Some(tag)).collect();
            let Some(index) = same.iter().position(|&n| n == id) else { return false };
            let pos = if *from_end { same.len() - index } else { index + 1 };
            nth_fits(*a, *b, pos as i64)
        }
        Pseudo::Link => matches!(tag, "a" | "area") && attrs.contains_key("href"),
        Pseudo::Root => dom.nodes[id].parent.is_none_or(|p| !is_element(dom, p)),
        Pseudo::Not(args) => !args.iter().any(|c| compound_matches(c, dom, id)),
    }
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
        && c.attrs.iter().all(|a| attr_matches(a, attrs))
        && c.pseudos.iter().all(|p| pseudo_matches(p, dom, id, tag, attrs))
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
        Comb::Adjacent | Comb::Sibling => {
            let Some(parent) = p else { return false };
            let siblings: Vec<NodeId> = element_children(dom, parent).collect();
            let Some(index) = siblings.iter().position(|&n| n == id) else { return false };
            let before = &siblings[..index];
            if *comb == Comb::Adjacent {
                before.last().is_some_and(|&prev| match_from(dom, prev, sel, i - 1, memo, failed))
            } else {
                before.iter().rev().any(|&prev| match_from(dom, prev, sel, i - 1, memo, failed))
            }
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
        for sel_text in split_top_level_commas(&rest[..brace]) {
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
    fn selectors_that_need_unavailable_state_are_dropped_not_misapplied() {
        // :hover/:focus/:visited depend on state we don't keep; pseudo-elements aren't boxes.
        // Ignoring the rule is better than applying `a:hover` styles to every link.
        for s in [
            "a:hover", "a:focus", "a:active", "a:visited", "input:checked", "::before", "a::after", "p:first-line",
            "a:made-up", ".", "#", "a[href", "a[=x]", "a[href=]", "a[href='x]", "a[href!=x]", "a:nth-child()", "a:nth-child(foo)",
            "a:nth-child", "a:first-child(2)", "a:not()", "a:not(:hover)", "a:link(x)",
        ] {
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

    fn find(dom: &Dom, tag: &str, nth: usize) -> NodeId {
        (0..dom.nodes.len()).filter(|&i| dom.tag(i) == Some(tag)).nth(nth).unwrap_or_else(|| panic!("no {tag} #{nth}"))
    }

    fn matches_sel(dom: &Dom, id: NodeId, s: &str) -> bool {
        selector_matches(dom, id, &parse_selector(s).unwrap_or_else(|| panic!("{s} should parse")))
    }

    #[test]
    fn attribute_selectors() {
        let dom = html::parse(
            "<a id=l href='https://example.com/a.PDF' lang='en-GB' class='x y' data-k='alpha beta' title=''>t</a>\
             <input type=TEXT disabled>",
        );
        let a = find(&dom, "a", 0);
        for yes in [
            "[href]", "a[href]", "a[href='https://example.com/a.PDF']", "[href^='https://']", "[href$='.PDF']", "[href*='example']",
            "[lang|=en]", "[lang|='en-GB']", "[data-k~=beta]", "[data-k~='alpha']", "[title='']", "[href$='.pdf' i]", "[HREF]", "a[lang|=en][data-k~=alpha]",
        ] {
            assert!(matches_sel(&dom, a, yes), "{yes}");
        }
        for no in [
            "[src]", "[href='x']", "[href^='http://']", "[href$='.pdf']", "[href*='nope']", "[lang|=e]", "[lang|=GB]", "[data-k~='alp']",
            "[data-k~='alpha beta']", "[data-k~='']", "[href^='']", "[href$='']", "[href*='']",
        ] {
            assert!(!matches_sel(&dom, a, no), "{no}");
        }
        assert_eq!(parse_selector("a[href]").unwrap().specificity(), (0, 1, 1), "an attribute counts like a class");
        let input = find(&dom, "input", 0);
        assert!(matches_sel(&dom, input, "input[disabled]"), "valueless attributes exist");
    }

    #[test]
    fn structural_pseudo_classes() {
        let dom = html::parse("<ul><li>1</li> text <li>2</li><!-- c --><li>3</li><li>4</li><li>5</li></ul><p>a</p><p>b</p>");
        let li = |n| find(&dom, "li", n);
        let ok = |n: usize, s: &str| matches_sel(&dom, li(n), s);
        assert!(ok(0, "li:first-child") && !ok(1, "li:first-child") && ok(4, "li:last-child") && !ok(0, "li:last-child"));
        assert!(ok(1, "li:nth-child(2)") && !ok(2, "li:nth-child(2)"), "text and comments don't count as children");
        let odd: Vec<usize> = (0..5).filter(|&n| ok(n, "li:nth-child(odd)")).collect();
        assert_eq!(odd, [0, 2, 4]);
        let even: Vec<usize> = (0..5).filter(|&n| ok(n, "li:nth-child(even)")).collect();
        assert_eq!(even, [1, 3]);
        let tail: Vec<usize> = (0..5).filter(|&n| ok(n, "li:nth-child(n+3)")).collect();
        assert_eq!(tail, [2, 3, 4]);
        let head: Vec<usize> = (0..5).filter(|&n| ok(n, "li:nth-child(-n+2)")).collect();
        assert_eq!(head, [0, 1]);
        let threes: Vec<usize> = (0..5).filter(|&n| ok(n, "li:nth-child( 2n + 1 )")).collect();
        assert_eq!(threes, [0, 2, 4], "spaces inside the argument are fine");
        assert!(ok(4, "li:nth-last-child(1)") && ok(3, "li:nth-last-child(2)") && !ok(0, "li:nth-last-child(2)"));
        // of-type counts only elements of the same tag
        let dom2 = html::parse("<div><span>a</span><p>b</p><span>c</span></div>");
        let (s0, s1, p0) = (find(&dom2, "span", 0), find(&dom2, "span", 1), find(&dom2, "p", 0));
        assert!(matches_sel(&dom2, s0, "span:first-of-type") && matches_sel(&dom2, s1, "span:last-of-type"));
        assert!(matches_sel(&dom2, p0, "p:only-of-type") && !matches_sel(&dom2, s0, "span:only-of-type"));
        assert!(!matches_sel(&dom2, p0, "p:first-child") && matches_sel(&dom2, s0, "span:first-child"));
        // only-child needs no siblings at all
        let dom3 = html::parse("<div><b>x</b></div><div><b>x</b><i>y</i></div>");
        assert!(matches_sel(&dom3, find(&dom3, "b", 0), "b:only-child") && !matches_sel(&dom3, find(&dom3, "b", 1), "b:only-child"));
        // a pseudo-class counts like a class for specificity
        assert_eq!(parse_selector("li:first-child").unwrap().specificity(), (0, 1, 1));
    }

    #[test]
    fn link_root_and_not() {
        let dom = html::parse("<a href=x class=ext>l</a><a name=anchor>n</a><p class='a b'>p</p><p class=a>q</p>");
        let (link, anchor) = (find(&dom, "a", 0), find(&dom, "a", 1));
        assert!(matches_sel(&dom, link, "a:link") && matches_sel(&dom, link, ":any-link"));
        assert!(!matches_sel(&dom, anchor, "a:link"), "an anchor without href isn't a link");
        let html_el = find(&dom, "html", 0);
        assert!(matches_sel(&dom, html_el, ":root") && !matches_sel(&dom, link, ":root"));
        let (p0, p1) = (find(&dom, "p", 0), find(&dom, "p", 1));
        assert!(matches_sel(&dom, p1, "p:not(.b)") && !matches_sel(&dom, p0, "p:not(.b)"));
        assert!(matches_sel(&dom, p1, "p:not(.b, .c)") && !matches_sel(&dom, p0, "p:not(.b, .c)"), "a list excludes anything matching any entry");
        assert!(matches_sel(&dom, anchor, "a:not([href])") && !matches_sel(&dom, link, "a:not([href])"));
        assert!(!matches_sel(&dom, link, "a:not(:first-child):not(.nope)"), "the link is the first child of its parent");
        // :not counts as its most specific argument
        assert_eq!(parse_selector("p:not(#x)").unwrap().specificity(), (1, 0, 1));
        assert_eq!(parse_selector("p:not(.a, #x)").unwrap().specificity(), (1, 0, 1));
        assert_eq!(parse_selector("a:link").unwrap().specificity(), (0, 1, 1));
    }

    #[test]
    fn sibling_combinators() {
        let dom = html::parse("<div><h1>t</h1><p>a</p><!--c--> text <p>b</p><span>s</span><p>c</p></div>");
        let p = |n| find(&dom, "p", n);
        let span = find(&dom, "span", 0);
        let m = |id, s: &str| matches_sel(&dom, id, s);
        assert!(m(p(0), "h1 + p") && !m(p(1), "h1 + p") && !m(p(2), "h1 + p"));
        assert!(m(p(1), "p + p"), "text and comments between siblings don't break adjacency");
        assert!(!m(p(2), "p + p"), "the span sits between");
        assert!(m(span, "p + span") && m(p(2), "span + p"));
        assert!(m(p(0), "h1 ~ p") && m(p(1), "h1 ~ p") && m(p(2), "h1 ~ p") && !m(p(0), "span ~ p") && m(p(2), "span ~ p"));
        assert!(m(p(1), "div > h1 + p ~ p"), "combinators chain");
        assert!(m(p(2), "h1~p") && m(p(0), "h1+p") && m(p(0), "h1 +p"), "spaces around + and ~ are optional");
        // the first child has no previous sibling
        assert!(!m(find(&dom, "h1", 0), "p + h1"));
        // a dangling or doubled sibling combinator is invalid
        for bad in ["+ p", "p +", "p ~ ~ q", "p + > q", "~"] {
            assert!(parse_selector(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn plus_and_tilde_inside_arguments_are_not_combinators() {
        assert!(parse_selector("li:nth-child(2n+1)").is_some());
        assert!(parse_selector("a[href~='x y'] > b").is_some());
        assert!(parse_selector("a[title='a > b + c']").is_some());
        let sel = parse_selector("ul > li:nth-child(2n+1) + li a[rel~=nofollow]").unwrap();
        assert_eq!(sel.parts.len(), 4);
        // tags ul, li, li, a; classes: the :nth-child and the attribute
        assert_eq!(sel.specificity(), (0, 2, 4));
    }

    #[test]
    fn selector_lists_split_only_at_top_level_commas() {
        let mut rules = vec![];
        parse_stylesheet("h1, p:not(.a, .b), a[title='x,y'] { color: red } b:hover, i { color: blue }", &mut rules);
        // h1, p:not(...), a[...] and i (b:hover is dropped on its own, not the whole rule)
        assert_eq!(rules.len(), 4);
        let dom = html::parse("<p class=c>x</p><p class=a>y</p>");
        let hits = |i: usize| rules.iter().filter(|r| selector_matches(&dom, i, &r.selector)).count();
        assert_eq!(hits(find(&dom, "p", 0)), 1);
        assert_eq!(hits(find(&dom, "p", 1)), 0, ":not(.a, .b) excludes it");
    }

    #[test]
    fn sibling_selectors_stay_fast_on_wide_trees() {
        // `li + li ~ li:nth-child(odd)` on 3000 siblings: each element looks at its earlier siblings once
        let src = format!("<ul>{}</ul>", "<li>x</li>".repeat(3000));
        let dom = html::parse(&src);
        let sel = parse_selector("li + li ~ li:nth-child(odd)").unwrap();
        let t = std::time::Instant::now();
        let n = (0..dom.nodes.len()).filter(|&i| dom.tag(i) == Some("li") && selector_matches(&dom, i, &sel)).count();
        assert!(n > 1000, "{n}");
        assert!(t.elapsed().as_secs() < 10, "{:?}", t.elapsed());
    }

    #[test]
    fn stylesheet_skips_at_rules_and_survives_garbage() {
        let mut rules = vec![];
        parse_stylesheet("@import url(x.css); @media print { p { color: red } } b { color: blue } } {{ i { color: green }", &mut rules);
        assert!(rules.iter().any(|r| r.selector.specificity() == (0, 0, 1)));
        assert!(!rules.is_empty());
        for s in ["", "{", "}", "a{", "@", "@media", "/* unterminated", "a { color: }", "a[", "a:not(", "a:nth-child(", "[", ":", "::", "a[b='"] {
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
        parse_stylesheet("a.x { content: \"}\"; } p { color: red }", &mut rules);
        assert!(rules.iter().any(|r| r.selector.specificity() == (0, 0, 1) && r.decls.iter().any(|d| d.name == "color")));
        let mut rules = vec![];
        parse_stylesheet("@media x { a { content: \"}\" } } p { color: blue }", &mut rules);
        assert_eq!(rules.len(), 1);
        // a rule for a pseudo-element is dropped, but the block after it is still skipped properly
        let mut rules = vec![];
        parse_stylesheet("a::before { content: \"}\"; } p { color: red }", &mut rules);
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
