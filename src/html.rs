//! Stage 1: HTML text -> DOM. A forgiving hand-written tokenizer/tree builder
//! (nowhere near the full HTML5 algorithm, but handles real-world basics).
use crate::dom::{Dom, Kind, NodeId};
use std::collections::HashMap;

pub const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source",
    "track", "wbr",
];
const MAX_DEPTH: usize = 512;
/// Elements that belong in <head>; they don't trigger an implicit <body>.
const HEAD_TAGS: &[&str] = &["title", "meta", "link", "style", "script", "base", "template"];
const RAW_TEXT: &[&str] = &["script", "style", "title", "textarea"];
/// Opening one of these while a <p> is open implicitly closes the <p>.
const CLOSES_P: &[&str] = &[
    "p", "div", "ul", "ol", "h1", "h2", "h3", "h4", "h5", "h6", "table", "pre", "blockquote",
    "hr", "li", "dl", "dt", "dd", "section", "article", "header", "footer", "nav", "form",
    "aside", "main", "fieldset", "address", "details", "figure", "figcaption", "center",
    "menu", "summary", "hgroup",
];

pub fn parse(src: &str) -> Dom {
    let src = src.strip_prefix('\u{feff}').unwrap_or(src); // a byte-order mark is not content
    let mut dom = Dom::new();
    let mut stack: Vec<NodeId> = vec![0];
    let lower = src.to_ascii_lowercase(); // same byte offsets as `src`
    let mut i = 0;

    while i < src.len() {
        let rest = &src[i..];
        if rest.starts_with("<!--") {
            i = rest.find("-->").map_or(src.len(), |e| i + e + 3);
            continue;
        }
        // doctype, CDATA-ish `<!`, and `<?xml ...?>` are all skipped like bogus comments
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = rest.find('>').map_or(src.len(), |e| i + e + 1);
            continue;
        }
        if rest.starts_with('<') {
            match scan_tag(src, i) {
                Scan::Tag(end) => {
                    let inner = &src[i + 1..end];
                    i = end + 1;
                    handle_tag(&mut dom, &mut stack, inner, &lower, &mut i, src);
                    continue;
                }
                // Like HTML5: EOF inside a tag drops the tag. (Re-scanning from
                // every later `<` instead would be quadratic on adversarial input.)
                Scan::Eof => break,
                Scan::NotATag => {} // stray '<': fall through and treat as text
            }
        }
        // Skip the first *character* (not byte!) so a stray '<' or a multi-byte
        // char can't make us slice inside a UTF-8 sequence.
        let first = rest.chars().next().map_or(1, char::len_utf8);
        let next = rest[first..].find('<').map_or(src.len(), |e| i + first + e);
        let text = decode_entities(&src[i..next]);
        if !text.trim().is_empty() {
            ensure_body(&mut dom, &mut stack);
        }
        let parent = *stack.last().unwrap();
        dom.add(parent, Kind::Text(text));
        i = next;
    }
    dom
}

fn body_of(dom: &Dom) -> Option<NodeId> {
    dom.nodes[0].children.iter().copied().find(|&c| dom.tag(c) == Some("body"))
}

/// Flow content (or text) has started: leave `<head>` if it's still open and,
/// like HTML5, synthesize `<body>` if the document didn't write one. Without
/// this, `body { ... }` rules and the default 8px body margin wouldn't apply
/// to fragments, and an unclosed `<head>` would swallow the whole page.
fn ensure_body(dom: &mut Dom, stack: &mut Vec<NodeId>) {
    if dom.tag(*stack.last().unwrap()) == Some("head") {
        stack.pop();
    }
    if stack.len() == 1 && body_of(dom).is_none() {
        let b = dom.add(0, Kind::Element { tag: "body".into(), attrs: HashMap::new() });
        stack.push(b);
    }
}

/// A `<td>`/`<th>` written directly inside `<table>` (or a row group) with no
/// `<tr>` gets an implied row, as browsers do. Without it the cell has no row
/// to live in and its content would vanish from the layout.
fn ensure_row(dom: &mut Dom, stack: &mut Vec<NodeId>) {
    let mut needs_row = false;
    for &n in stack.iter().rev() {
        match dom.tag(n) {
            Some("tr") => break,
            Some("table" | "thead" | "tbody" | "tfoot") => {
                needs_row = true;
                break;
            }
            _ => {}
        }
    }
    if needs_row {
        let parent = *stack.last().unwrap();
        let tr = dom.add(parent, Kind::Element { tag: "tr".into(), attrs: HashMap::new() });
        stack.push(tr);
    }
}

enum Scan {
    /// Index of the closing `>`.
    Tag(usize),
    /// A tag starts here but the input ends before it closes.
    Eof,
    /// `<` not followed by a letter or `/letter`: plain text.
    NotATag,
}

/// Find the end of the tag starting at `start`. As in HTML5, a quote only
/// opens a quoted attribute value when it directly follows `=` — so
/// `<div class=don't>` is a normal tag.
fn scan_tag(src: &str, start: usize) -> Scan {
    let b = src.as_bytes();
    let mut j = start + 1;
    if j < b.len() && b[j] == b'/' {
        j += 1;
    }
    if j >= b.len() || !b[j].is_ascii_alphabetic() {
        return Scan::NotATag;
    }
    let mut quote: Option<u8> = None;
    let mut last = 0u8; // last non-whitespace byte seen outside a quoted value
    while j < b.len() {
        let c = b[j];
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                    last = c;
                }
            }
            None => match c {
                b'>' => return Scan::Tag(j),
                b'"' | b'\'' if last == b'=' => quote = Some(c),
                c if c.is_ascii_whitespace() => {}
                _ => last = c,
            },
        }
        j += 1;
    }
    Scan::Eof
}

/// Pop the open-element stack back to (and including) the nearest element
/// named in `targets`, unless an element in `stop` (a scope boundary) is hit first.
fn pop_to(dom: &Dom, stack: &mut Vec<NodeId>, targets: &[&str], stop: &[&str]) {
    for ix in (1..stack.len()).rev() {
        let t = dom.tag(stack[ix]).unwrap_or("");
        if targets.contains(&t) {
            stack.truncate(ix);
            return;
        }
        if stop.contains(&t) {
            return;
        }
    }
}

/// Implied end tags: opening `tag` may implicitly close elements that are
/// still open (an unclosed <p>, <li>, <td>, ...), however deep they are.
fn close_implied(dom: &Dom, stack: &mut Vec<NodeId>, tag: &str) {
    if CLOSES_P.contains(&tag) {
        pop_to(dom, stack, &["p"], &["table", "td", "th", "caption", "button", "object"]);
    }
    match tag {
        "li" => pop_to(dom, stack, &["li"], &["ul", "ol", "menu"]),
        "dt" | "dd" => pop_to(dom, stack, &["dt", "dd"], &["dl"]),
        "tr" => pop_to(dom, stack, &["tr"], &["table"]),
        "td" | "th" => pop_to(dom, stack, &["td", "th"], &["tr", "table"]),
        "thead" | "tbody" | "tfoot" => pop_to(dom, stack, &["thead", "tbody", "tfoot"], &["table"]),
        "option" => pop_to(dom, stack, &["option"], &["select", "datalist", "optgroup"]),
        "optgroup" => {
            // a new group ends the open option and the open group
            pop_to(dom, stack, &["option"], &["optgroup", "select", "datalist"]);
            pop_to(dom, stack, &["optgroup"], &["select", "datalist"]);
        }
        "a" => pop_to(dom, stack, &["a"], &[]),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let top = dom.tag(*stack.last().unwrap()).unwrap_or("");
            if matches!(top, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") && stack.len() > 1 {
                stack.pop();
            }
        }
        _ => {}
    }
}

/// Byte offset of the `</tag` that ends a raw-text element (must be followed by
/// whitespace, `/` or `>`, so `</scriptx>` doesn't count), or the end of input.
fn find_end_tag(lower: &str, from: usize, tag: &str) -> usize {
    let needle = format!("</{tag}");
    let mut from = from;
    while let Some(e) = lower[from..].find(&needle) {
        let at = from + e;
        match lower.as_bytes().get(at + needle.len()) {
            None | Some(b'>') | Some(b'/') => return at,
            Some(c) if c.is_ascii_whitespace() => return at,
            _ => from = at + needle.len(),
        }
    }
    lower.len()
}

fn handle_tag(
    dom: &mut Dom,
    stack: &mut Vec<NodeId>,
    inner: &str,
    lower: &str,
    pos: &mut usize,
    src: &str,
) {
    let closing = inner.starts_with('/');
    let body = if closing { &inner[1..] } else { inner };
    let name_len = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .unwrap_or(body.len());
    let tag = body[..name_len].to_ascii_lowercase();
    let attr_src = &body[name_len..];

    if closing {
        match tag.as_str() {
            // Browsers ignore these: anything after them still belongs to <body>.
            "body" | "html" => return,
            // `</br>` means `<br>`; a stray `</p>` makes an empty paragraph.
            "br" => {
                ensure_body(dom, stack);
                let parent = *stack.last().unwrap();
                dom.add(parent, Kind::Element { tag: "br".into(), attrs: HashMap::new() });
                return;
            }
            "p" if !stack.iter().any(|&n| dom.tag(n) == Some("p")) => {
                ensure_body(dom, stack);
                let parent = *stack.last().unwrap();
                dom.add(parent, Kind::Element { tag: "p".into(), attrs: HashMap::new() });
                return;
            }
            _ => {}
        }
        if let Some(idx) = stack.iter().rposition(|&n| dom.tag(n) == Some(tag.as_str())) {
            if idx > 0 {
                stack.truncate(idx);
            }
        }
        return;
    }
    if tag == "html" {
        if let Kind::Element { attrs, .. } = &mut dom.nodes[0].kind {
            for (k, v) in parse_attrs(attr_src) {
                attrs.entry(k).or_insert(v);
            }
        }
        return;
    }

    if tag == "body" {
        // An explicit <body> ends an unclosed <head>. If an implicit body was
        // already opened, reuse it instead of creating a second one.
        stack.truncate(1);
        if let Some(b) = body_of(dom) {
            if let Kind::Element { attrs, .. } = &mut dom.nodes[b].kind {
                for (k, v) in parse_attrs(attr_src) {
                    attrs.entry(k).or_insert(v);
                }
            }
            stack.push(b);
            return;
        }
    } else if tag != "head" && !HEAD_TAGS.contains(&tag.as_str()) {
        ensure_body(dom, stack);
    }

    close_implied(dom, stack, &tag);
    if tag == "td" || tag == "th" {
        ensure_row(dom, stack);
    }

    let parent = *stack.last().unwrap();
    let id = dom.add(parent, Kind::Element { tag: tag.clone(), attrs: parse_attrs(attr_src) });

    // Per HTML5 a trailing `/` only means something on void elements; `<div/>`
    // is just an opening tag.
    if VOID.contains(&tag.as_str()) {
        return;
    }
    if RAW_TEXT.contains(&tag.as_str()) {
        let end = find_end_tag(lower, *pos, &tag);
        if end > *pos {
            let raw = &src[*pos..end];
            let text = if tag == "script" || tag == "style" { raw.to_string() } else { decode_entities(raw) };
            dom.add(id, Kind::Text(text));
        }
        *pos = src[end..].find('>').map_or(src.len(), |e| end + e + 1);
        return;
    }
    // Like real browsers, cap nesting depth so pathological input can't blow
    // the stack in the recursive stages. Past the cap, children attach to the
    // deepest open element instead.
    if stack.len() < MAX_DEPTH {
        stack.push(id);
    }
}

fn parse_attrs(s: &str) -> HashMap<String, String> {
    let c: Vec<char> = s.chars().collect();
    let mut attrs = HashMap::new();
    let mut i = 0;
    while i < c.len() {
        while i < c.len() && (c[i].is_whitespace() || c[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < c.len() && !c[i].is_whitespace() && c[i] != '=' && c[i] != '/' {
            i += 1;
        }
        if start == i {
            break;
        }
        let name: String = c[start..i].iter().collect::<String>().to_ascii_lowercase();
        while i < c.len() && c[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < c.len() && c[i] == '=' {
            i += 1;
            while i < c.len() && c[i].is_whitespace() {
                i += 1;
            }
            if i < c.len() && (c[i] == '"' || c[i] == '\'') {
                let q = c[i];
                i += 1;
                let vs = i;
                while i < c.len() && c[i] != q {
                    i += 1;
                }
                value = c[vs..i].iter().collect();
                i += 1;
            } else {
                let vs = i;
                while i < c.len() && !c[i].is_whitespace() {
                    i += 1;
                }
                value = c[vs..i].iter().collect();
            }
        }
        // Duplicate attributes: the first one wins, as in browsers.
        attrs.entry(name).or_insert_with(|| decode_entities(&value));
    }
    attrs
}

pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match decode_one(rest) {
            Some((c, consumed)) => {
                out.push(c);
                rest = &rest[consumed..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Entities that browsers also decode without a trailing `;`.
const LEGACY: &[(&str, char)] =
    &[("amp", '&'), ("lt", '<'), ("gt", '>'), ("quot", '"'), ("nbsp", '\u{a0}'), ("copy", '©'), ("reg", '®')];

/// Decode the entity at the start of `rest` (which begins with `&`).
/// Returns the character and how many bytes it used. Only a small bounded
/// window is searched for the `;`, so a long run of `&` stays linear.
fn decode_one(rest: &str) -> Option<(char, usize)> {
    let b = rest.as_bytes();
    let window = &b[1..b.len().min(34)];
    if let Some(semi) = window.iter().position(|&c| c == b';') {
        if let Some(c) = entity(&rest[1..1 + semi]) {
            return Some((c, semi + 2));
        }
    }
    for (name, c) in LEGACY {
        if rest[1..].starts_with(name) && !b.get(1 + name.len()).is_some_and(|n| n.is_ascii_alphanumeric()) {
            return Some((*c, 1 + name.len()));
        }
    }
    None
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(h) => (h, 16),
            None => (num, 10),
        };
        // from_str_radix would also accept a leading '+', which browsers don't.
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let code = u32::from_str_radix(digits, radix).unwrap_or(u32::MAX);
        // NUL, surrogates and out-of-range values become U+FFFD, as in HTML5.
        return Some(if code == 0 { '\u{FFFD}' } else { char::from_u32(code).unwrap_or('\u{FFFD}') });
    }
    Some(match name {
        "amp" => '&', "lt" => '<', "gt" => '>', "quot" => '"', "apos" => '\'',
        "nbsp" => '\u{a0}', "copy" => '©', "reg" => '®', "trade" => '™', "mdash" => '—',
        "ndash" => '–', "hellip" => '…', "lsquo" => '‘', "rsquo" => '’', "ldquo" => '“',
        "rdquo" => '”', "bull" => '•', "middot" => '·', "laquo" => '«', "raquo" => '»',
        "times" => '×', "divide" => '÷', "rarr" => '→', "larr" => '←', "uarr" => '↑',
        "darr" => '↓', "euro" => '€', "pound" => '£', "yen" => '¥', "cent" => '¢',
        "deg" => '°', "plusmn" => '±', "frac12" => '½', "frac14" => '¼', "frac34" => '¾',
        "sect" => '§', "para" => '¶', "dagger" => '†', "infin" => '∞', "ne" => '≠',
        "le" => '≤', "ge" => '≥', "hearts" => '♥', "check" => '✓', "thinsp" => '\u{2009}',
        "ensp" => '\u{2002}', "emsp" => '\u{2003}', "iexcl" => '¡', "iquest" => '¿',
        "eacute" => 'é', "egrave" => 'è', "aacute" => 'á', "agrave" => 'à', "iacute" => 'í',
        "oacute" => 'ó', "uacute" => 'ú', "ntilde" => 'ñ', "ccedil" => 'ç', "szlig" => 'ß',
        "auml" => 'ä', "ouml" => 'ö', "uuml" => 'ü', "Auml" => 'Ä', "Ouml" => 'Ö', "Uuml" => 'Ü',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(dom: &Dom, id: NodeId) -> Vec<String> {
        dom.nodes[id].children.iter().filter_map(|&c| dom.tag(c).map(String::from)).collect()
    }

    fn body(dom: &Dom) -> NodeId {
        dom.nodes[0].children.iter().copied().find(|&c| dom.tag(c) == Some("body")).expect("document has a <body>")
    }

    #[test]
    fn text_starting_with_multibyte_char_does_not_panic() {
        // regression: slicing at byte 1 inside 'é' used to panic
        let dom = parse("é日本語 <b>ü</b> 🎉");
        assert_eq!(dom.text_content(0), "é日本語 ü 🎉");
    }

    #[test]
    fn pathological_inputs_do_not_panic_or_hang() {
        for s in ["", "<", "text <", "<div class=\"a", "<!-- never ends", "</", "<a href=>", "&", "&#;", "&#xZZ;"] {
            parse(s);
        }
    }

    #[test]
    fn p_is_implicitly_closed_by_block_tags() {
        let dom = parse("<p>one<div>two</div>");
        assert_eq!(tags(&dom, body(&dom)), ["p", "div"]);
    }

    #[test]
    fn li_closes_previous_li() {
        let dom = parse("<ul><li>a<li>b</ul>");
        let ul = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, ul), ["li", "li"]);
    }

    #[test]
    fn style_contents_are_raw_text() {
        let dom = parse("<style>a > b { color: red } p::before{content:'<b>'}</style><p>x</p>");
        assert_eq!(tags(&dom, 0), ["style", "body"]);
        assert_eq!(tags(&dom, body(&dom)), ["p"]);
        assert!(dom.text_content(dom.nodes[0].children[0]).contains("a > b"));
    }

    #[test]
    fn misnested_end_tags_recover() {
        let dom = parse("<div><span>a</div>b</span>c");
        assert_eq!(dom.text_content(0), "abc");
    }

    #[test]
    fn attributes_parse_in_all_forms() {
        let dom = parse("<a href=x title='a b' data-q=\"1>2\" hidden>t</a>");
        let a = dom.nodes[body(&dom)].children[0];
        assert_eq!(dom.attr(a, "href"), Some("x"));
        assert_eq!(dom.attr(a, "title"), Some("a b"));
        assert_eq!(dom.attr(a, "data-q"), Some("1>2"));
        assert_eq!(dom.attr(a, "hidden"), Some(""));
    }

    #[test]
    fn entities_decode() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#65;&#x42; &bogus; &"), "a & b <c> AB &bogus; &");
    }

    #[test]
    fn nesting_depth_is_capped() {
        let n = 5000;
        let dom = parse(&format!("{}x{}", "<div>".repeat(n), "</div>".repeat(n)));
        fn depth(dom: &Dom, id: NodeId) -> usize {
            1 + dom.nodes[id].children.iter().map(|&c| depth(dom, c)).max().unwrap_or(0)
        }
        assert!(depth(&dom, 0) <= MAX_DEPTH + 2);
        assert_eq!(dom.text_content(0), "x");
    }

    #[test]
    fn body_is_synthesised_for_fragments() {
        let dom = parse("<title>t</title><h1>hi</h1>");
        assert_eq!(tags(&dom, 0), ["title", "body"]);
        assert_eq!(tags(&dom, body(&dom)), ["h1"]);
    }

    #[test]
    fn unclosed_head_does_not_swallow_the_page() {
        let dom = parse("<head><title>t</title><body><p>shown</p></body>");
        assert_eq!(tags(&dom, 0), ["head", "body"]);
        assert_eq!(dom.text_content(body(&dom)), "shown");
        // flow content with no <body> tag also leaves an open <head>
        let dom = parse("<head><title>t</title><p>shown</p>");
        assert_eq!(tags(&dom, 0), ["head", "body"]);
    }

    #[test]
    fn explicit_body_reuses_implicit_one() {
        let dom = parse("text<body class=x>more");
        let bodies = tags(&dom, 0).iter().filter(|t| *t == "body").count();
        assert_eq!(bodies, 1);
        assert_eq!(dom.attr(body(&dom), "class"), Some("x"));
        assert_eq!(dom.text_content(body(&dom)), "textmore");
    }

    // ---------- regressions from the adversarial review ----------

    fn time_it(f: impl FnOnce()) -> std::time::Duration {
        let t = std::time::Instant::now();
        f();
        t.elapsed()
    }

    #[test]
    fn adversarial_inputs_are_linear_time() {
        // These used to take 5-30s (quadratic).
        for input in ["&".repeat(300_000), "&a".repeat(150_000), "<a ".repeat(100_000), "<a \"".repeat(80_000), "<div ".repeat(60_000)] {
            let d = time_it(|| { parse(&input); });
            assert!(d.as_secs_f32() < 2.0, "{:?} on {:?}...", d, &input[..6]);
        }
    }

    #[test]
    fn eof_inside_a_tag_drops_the_tag() {
        let dom = parse("<p>kept</p><div class=\"never closed");
        assert_eq!(dom.text_content(0), "kept");
    }

    #[test]
    fn bom_and_xml_declaration_are_ignored() {
        let dom = parse("\u{feff}<?xml version=\"1.0\"?><!doctype html><html><head><title>t</title></head><body><p>hi</p></body></html>");
        assert_eq!(tags(&dom, 0), ["head", "body"]);
        assert_eq!(dom.text_content(body(&dom)), "hi");
    }

    #[test]
    fn implied_end_tags_close_through_nested_elements() {
        // rows and cells become siblings
        let dom = parse("<table><tr><td>a<tr><td>b</table>");
        let table = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, table), ["tr", "tr"]);
        // <li><p> ... <li>
        let dom = parse("<ul><li><p>a<li><p>b</ul>");
        let ul = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, ul), ["li", "li"]);
        // a block closes an open <p> even with an inline element in between
        let dom = parse("<p><b>x<div>y</div></b>");
        assert_eq!(tags(&dom, body(&dom)), ["p", "div"]);
        // headings and anchors don't nest
        let dom = parse("<h1>a<h2>b");
        assert_eq!(tags(&dom, body(&dom)), ["h1", "h2"]);
        let dom = parse("<a href=1>x<a href=2>y");
        assert_eq!(tags(&dom, body(&dom)), ["a", "a"]);
        // but a nested list's <li> doesn't close the outer one
        let dom = parse("<ul><li>a<ul><li>b</ul></ul>");
        let ul = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, ul), ["li"]);
    }

    #[test]
    fn cells_without_a_row_get_an_implied_row() {
        // `<td>` straight inside <table> used to have no row, so its content vanished from the layout
        let dom = parse("<table><td>X</td><td>Y</td></table>");
        let table = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, table), ["tr"]);
        let tr = dom.nodes[table].children[0];
        assert_eq!(tags(&dom, tr), ["td", "td"]);
        // inside a row group, and a later <tr> closes the implied one
        let dom = parse("<table><tbody><th>h<tr><td>b</table>");
        let tbody = dom.nodes[dom.nodes[body(&dom)].children[0]].children[0];
        assert_eq!(tags(&dom, tbody), ["tr", "tr"]);
        // an ordinary row gets no extra wrapper
        let dom = parse("<table><tr><td>a<td>b</table>");
        let table = dom.nodes[body(&dom)].children[0];
        assert_eq!(tags(&dom, table), ["tr"]);
        assert_eq!(tags(&dom, dom.nodes[table].children[0]), ["td", "td"]);
        // a nested table inside a cell gets its own implied row
        let dom = parse("<table><tr><td><table><td>in</table></td></tr></table>");
        let outer_td = dom.nodes[dom.nodes[body(&dom)].children[0]].children[0];
        let outer_td = dom.nodes[outer_td].children[0];
        let inner_table = dom.nodes[outer_td].children[0];
        assert_eq!(tags(&dom, inner_table), ["tr"]);
    }

    #[test]
    fn content_after_end_body_and_end_html_stays_in_body() {
        let dom = parse("<body><p>a</p></body></html><p>after</p>text");
        assert_eq!(tags(&dom, 0), ["body"]);
        assert_eq!(tags(&dom, body(&dom)), ["p", "p"]);
        assert_eq!(dom.text_content(body(&dom)), "aaftertext");
    }

    #[test]
    fn trailing_slash_only_self_closes_void_elements() {
        let dom = parse("<div/>after");
        assert_eq!(dom.text_content(dom.nodes[body(&dom)].children[0]), "after");
        let dom = parse("<script src=x/>y</script><p>z</p>");
        assert_eq!(dom.text_content(body(&dom)), "z");
        let dom = parse("<br/><img src=a.png/>");
        assert_eq!(tags(&dom, body(&dom)), ["br", "img"]);
    }

    #[test]
    fn quote_inside_unquoted_value_does_not_open_a_string() {
        let dom = parse("<div class=don't>x</div><p>y</p>");
        assert_eq!(tags(&dom, body(&dom)), ["div", "p"]);
        let dom = parse("<a title = \"a > b\" href=x>t</a>");
        assert_eq!(dom.text_content(body(&dom)), "t");
    }

    #[test]
    fn entity_edge_cases() {
        assert_eq!(decode_entities("&amp &copy &lt;b&gt"), "& © <b>");
        assert_eq!(decode_entities("&copyright"), "&copyright"); // not a legacy match
        assert_eq!(decode_entities("&#0;|&#xD800;|&#x110000;|&#99999999999;"), "\u{fffd}|\u{fffd}|\u{fffd}|\u{fffd}");
        assert_eq!(decode_entities("&#x+41;|&#+65;|&#;|&#x;"), "&#x+41;|&#+65;|&#;|&#x;");
        assert_eq!(decode_entities("&eacute;&trade;&hearts;"), "é™♥");
    }

    #[test]
    fn nits_duplicate_attrs_br_end_tag_script_boundary() {
        let dom = parse("<p class=a class=b>x</p>");
        assert_eq!(dom.attr(dom.nodes[body(&dom)].children[0], "class"), Some("a"));
        let dom = parse("a</br>b");
        assert_eq!(tags(&dom, body(&dom)), ["br"]);
        let dom = parse("</p>x");
        assert_eq!(tags(&dom, body(&dom)), ["p"]);
        let dom = parse("<script>var a = '</scriptx>';</script><p>z</p>");
        assert_eq!(dom.text_content(dom.nodes[body(&dom)].children[0]), "z");
    }
}
