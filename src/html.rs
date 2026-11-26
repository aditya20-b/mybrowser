//! Stage 1: HTML text -> DOM. A forgiving hand-written tokenizer/tree builder
//! (nowhere near the full HTML5 algorithm, but handles real-world basics).
use crate::dom::{Dom, Kind, NodeId};
use std::collections::HashMap;

const VOID: &[&str] = &[
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
    "hr", "li", "dl", "section", "article", "header", "footer", "nav", "form", "aside", "main",
];

pub fn parse(src: &str) -> Dom {
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
        if rest.starts_with("<!") {
            i = rest.find('>').map_or(src.len(), |e| i + e + 1);
            continue;
        }
        if rest.starts_with('<') {
            if let Some(end) = tag_end(src, i) {
                let inner = &src[i + 1..end];
                i = end + 1;
                handle_tag(&mut dom, &mut stack, inner, &lower, &mut i, src);
                continue;
            }
            // stray '<': fall through and treat as text
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

/// If a real tag starts at `start` (`<` followed by a letter or `/letter`),
/// return the index of its closing `>` (respecting quoted attribute values).
fn tag_end(src: &str, start: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut j = start + 1;
    if j < b.len() && b[j] == b'/' {
        j += 1;
    }
    if j >= b.len() || !b[j].is_ascii_alphabetic() {
        return None;
    }
    let mut quote: Option<u8> = None;
    while j < b.len() {
        match (quote, b[j]) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"') | (None, b'\'') => quote = Some(b[j]),
            (None, b'>') => return Some(j),
            _ => {}
        }
        j += 1;
    }
    None
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
        if let Some(idx) = stack.iter().rposition(|&n| dom.tag(n) == Some(tag.as_str())) {
            if idx > 0 {
                stack.truncate(idx);
            }
        }
        return;
    }
    if tag == "html" {
        if let Kind::Element { attrs, .. } = &mut dom.nodes[0].kind {
            attrs.extend(parse_attrs(attr_src));
        }
        return;
    }

    if tag == "body" {
        // An explicit <body> ends an unclosed <head>. If an implicit body was
        // already opened, reuse it instead of creating a second one.
        stack.truncate(1);
        if let Some(b) = body_of(dom) {
            if let Kind::Element { attrs, .. } = &mut dom.nodes[b].kind {
                attrs.extend(parse_attrs(attr_src));
            }
            stack.push(b);
            return;
        }
    } else if tag != "head" && !HEAD_TAGS.contains(&tag.as_str()) {
        ensure_body(dom, stack);
    }

    // Implied end tags.
    let top = dom.tag(*stack.last().unwrap()).unwrap_or("");
    let pop = (top == "p" && CLOSES_P.contains(&tag.as_str()))
        || (top == "li" && tag == "li")
        || ((top == "dt" || top == "dd") && (tag == "dt" || tag == "dd"))
        || (top == "tr" && tag == "tr")
        || ((top == "td" || top == "th") && (tag == "td" || tag == "th" || tag == "tr"));
    if pop && stack.len() > 1 {
        stack.pop();
    }

    let self_closing = attr_src.trim_end().ends_with('/');
    let parent = *stack.last().unwrap();
    let id = dom.add(parent, Kind::Element { tag: tag.clone(), attrs: parse_attrs(attr_src) });

    if VOID.contains(&tag.as_str()) || self_closing {
        return;
    }
    if RAW_TEXT.contains(&tag.as_str()) {
        let needle = format!("</{tag}");
        let end = lower[*pos..].find(&needle).map_or(src.len(), |e| *pos + e);
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
        attrs.insert(name, decode_entities(&value));
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
        let semi = rest.find(';').filter(|&e| e <= 10);
        let decoded = semi.and_then(|e| entity(&rest[1..e]).map(|c| (c, e)));
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &rest[e + 1..];
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

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&', "lt" => '<', "gt" => '>', "quot" => '"', "apos" => '\'',
        "nbsp" => '\u{a0}', "copy" => '©', "reg" => '®', "mdash" => '—', "ndash" => '–',
        "hellip" => '…', "lsquo" => '‘', "rsquo" => '’', "ldquo" => '“', "rdquo" => '”',
        "bull" => '•', "middot" => '·', "laquo" => '«', "raquo" => '»', "times" => '×',
        "rarr" => '→', "larr" => '←', "euro" => '€', "pound" => '£', "deg" => '°',
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
}
