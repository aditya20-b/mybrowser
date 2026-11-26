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
            border_on: [false; 4],
            deco_own: false,
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
}

fn side(name: &str) -> Option<usize> {
    ["top", "right", "bottom", "left"].iter().position(|s| *s == name)
}

fn is_inherited(name: &str) -> bool {
    matches!(name, "color" | "font-size" | "font-weight" | "font-style" | "font-family"
        | "text-align" | "white-space" | "list-style" | "list-style-type")
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
    if let Some(rest) = name.strip_prefix("border-") {
        if let Some((sd, what)) = rest.split_once('-') {
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
            }
        }
        return;
    }

    match name {
        "display" => match lv.as_str() {
            "none" => s.display = Display::None,
            "list-item" => s.display = Display::ListItem,
            "inline" | "contents" | "table-cell" | "table-column" | "table-column-group" => s.display = Display::Inline,
            l if l.starts_with("inline-") => s.display = Display::Inline,
            "block" | "flex" | "grid" | "table" | "table-row" | "table-row-group"
            | "table-header-group" | "table-footer-group" | "table-caption" | "flow-root" => s.display = Display::Block,
            _ => {} // unknown value: ignore the declaration
        },
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
            s.deco_own = lv.contains("underline");
        }
        "text-align" => match lv.as_str() {
            "center" => s.align = Align::Center,
            "right" | "end" => s.align = Align::Right,
            "left" | "start" => s.align = Align::Left,
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

        if let Kind::Element { attrs, .. } = &dom.nodes[id].kind {
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

    // ---------- regressions from the adversarial review ----------

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
        assert_eq!(d("flex"), Display::Block);
        assert_eq!(d("bogus"), Display::Block); // ignored: keeps the UA default for <p>
        assert_eq!(style_of("<div style='display:inline'><p style='display:inherit'>x</p></div>", "p").display, Display::Inline);
    }
}
