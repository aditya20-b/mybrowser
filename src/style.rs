//! Stage 3: DOM + rules -> computed style for every node (cascade + inheritance).
use crate::css::{self, Color, Declaration, Length, Rule};
use crate::dom::{Dom, Kind};
use crate::fonts::{Face, Family};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Display {
    None,
    Inline,
    Block,
    ListItem,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Center,
    Right,
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
    // not inherited. Sides are ordered top, right, bottom, left.
    pub background: Option<Color>,
    pub margin: [Length; 4],
    pub padding: [Length; 4],
    pub border_w: [f32; 4],
    pub border_c: [Option<Color>; 4], // None = currentcolor
    pub width: Length,
    pub max_width: Option<f32>,
    pub height: Option<f32>,
    pub border_box: bool, // box-sizing: border-box (width/height include padding+border)
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
            background: None,
            margin: [Length::Px(0.0); 4],
            padding: [Length::Px(0.0); 4],
            border_w: [0.0; 4],
            border_c: [None; 4],
            width: Length::Auto,
            max_width: None,
            height: None,
            border_box: false,
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
            ..Style::initial()
        }
    }

    pub fn face(&self) -> Face {
        Face { family: self.family, bold: self.bold, italic: self.italic }
    }
}

fn side(name: &str) -> Option<usize> {
    ["top", "right", "bottom", "left"].iter().position(|s| *s == name)
}

fn apply(s: &mut Style, parent: &Style, d: &Declaration) {
    let (name, v) = (d.name.as_str(), d.value.trim());
    let lv = v.to_ascii_lowercase();
    let fs = s.font_size;

    if let Some(i) = name.strip_prefix("margin-").and_then(side) {
        if let Some(l) = css::parse_length(v, fs) { s.margin[i] = l; }
        return;
    }
    if let Some(i) = name.strip_prefix("padding-").and_then(side) {
        if let Some(l) = css::parse_length(v, fs) { s.padding[i] = l; }
        return;
    }
    if let Some(rest) = name.strip_prefix("border-") {
        if let Some((sd, what)) = rest.split_once('-') {
            if let Some(i) = side(sd) {
                match what {
                    "width" => {
                        if let Some(Length::Px(px)) = css::parse_length(v, fs) { s.border_w[i] = px.max(0.0); }
                    }
                    "color" => s.border_c[i] = css::parse_color(v),
                    _ => {}
                }
            }
        }
        return;
    }

    match name {
        "display" => {
            s.display = match lv.as_str() {
                "none" => Display::None,
                "inline" | "inline-block" | "inline-flex" | "table-cell" => Display::Inline,
                "list-item" => Display::ListItem,
                _ => Display::Block, // block, flex, grid, table, ... (best effort)
            }
        }
        "color" => { if let Some(c) = css::parse_color(v) { s.color = c; } }
        "background-color" => s.background = css::parse_color(v),
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
        "font-style" => s.italic = matches!(lv.as_str(), "italic" | "oblique"),
        "font-family" => {
            for f in lv.split(',') {
                let f = f.trim().trim_matches(|c| c == '"' || c == '\'');
                let fam = match f {
                    "monospace" | "courier" | "courier new" | "menlo" | "monaco" | "consolas"
                    | "sfmono-regular" => Some(Family::Mono),
                    "serif" | "georgia" | "times" | "times new roman" | "palatino" => Some(Family::Serif),
                    "sans-serif" | "arial" | "helvetica" | "helvetica neue" | "system-ui"
                    | "verdana" | "tahoma" | "-apple-system" | "segoe ui" => Some(Family::Sans),
                    _ => None,
                };
                if let Some(fam) = fam { s.family = fam; break; }
            }
        }
        "text-decoration" | "text-decoration-line" => {
            s.underline = lv.contains("underline");
        }
        "text-align" => {
            s.align = match lv.as_str() {
                "center" => Align::Center,
                "right" | "end" => Align::Right,
                _ => Align::Left,
            }
        }
        "white-space" => s.pre = matches!(lv.as_str(), "pre" | "pre-wrap" | "pre-line"),
        "width" => { if let Some(l) = css::parse_length(v, fs) { s.width = l; } }
        "max-width" => {
            s.max_width = match css::parse_length(v, fs) { Some(Length::Px(p)) => Some(p), _ => None };
        }
        "height" => {
            s.height = match css::parse_length(v, fs) { Some(Length::Px(p)) => Some(p), _ => None };
        }
        "box-sizing" => s.border_box = lv == "border-box",
        "list-style" | "list-style-type" => s.list_none = lv.contains("none"),
        _ => {}
    }
}

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

        if let Kind::Element { attrs, .. } = &dom.nodes[id].kind {
            // (important, specificity, order, declaration)
            let mut matched: Vec<((bool, (u32, u32, u32), usize), &Declaration)> = vec![];
            for rule in rules {
                if css::selector_matches(dom, id, &rule.selector) {
                    for d in rule.decls.iter() {
                        matched.push(((d.important, rule.selector.specificity(), rule.order), d));
                    }
                }
            }
            let inline = attrs.get("style").map(|t| css::parse_declarations(t)).unwrap_or_default();
            for d in &inline {
                matched.push(((d.important, (1000, 0, 0), usize::MAX), d));
            }
            matched.sort_by_key(|(k, _)| *k);

            // font-size first, so em units in other properties see the final size
            for (_, d) in matched.iter().filter(|(_, d)| d.name == "font-size") {
                apply(&mut s, parent_style, d);
            }
            for (_, d) in matched.iter().filter(|(_, d)| d.name != "font-size") {
                apply(&mut s, parent_style, d);
            }
        } else {
            s.display = Display::Inline;
        }
        styles.push(s);
    }
    styles
}

/// The browser's built-in default stylesheet.
pub const USER_AGENT_CSS: &str = "
head, style, script, title, meta, link, template, base { display: none }
html, body, div, section, article, header, footer, nav, main, aside, p, h1, h2, h3, h4, h5,
h6, ul, ol, dl, dt, dd, pre, blockquote, hr, form, figure, figcaption, fieldset, address,
details, summary, table, thead, tbody, tfoot, tr, caption { display: block }
li { display: list-item }
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
        css::parse_stylesheet(USER_AGENT_CSS, &mut rules);
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
}
