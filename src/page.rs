//! Ties the pipeline together for one document: fetch -> parse -> load
//! sub-resources -> style -> (layout, done per viewport width).
use crate::css::{self, Rule};
use crate::dom::{Dom, Kind, NodeId};
use crate::fonts::Fonts;
use crate::html;
use crate::images::{self, Image};
use crate::layout::{self, Layout};
use crate::net::{self, Fetched};
use crate::style::{self, Style};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use url::Url;

/// Upper bounds on sub-resources per page, so a hostile page can't make us
/// issue thousands of requests or exhaust memory.
const MAX_STYLESHEETS: usize = 32;
const MAX_IMAGES: usize = 300;
/// At most this many requests in flight at once.
const PARALLEL_FETCHES: usize = 8;
/// After this long, no new sub-resource requests are started.
const SUBRESOURCE_DEADLINE: Duration = Duration::from_secs(30);
/// Total bytes of sub-resource bodies we will hold at once.
const MAX_FETCHED_BYTES: u64 = 256 * 1024 * 1024;
/// Total decoded image memory per page.
const IMAGE_BUDGET: u64 = 256 * 1024 * 1024;
/// Longest document name / title we hand to the window system.
const MAX_TITLE_CHARS: usize = 120;

pub struct Page {
    pub url: Url,
    /// HTTP status of the document, if it came from the network.
    pub status: Option<u16>,
    pub title: String,
    pub dom: Dom,
    pub styles: Vec<Style>,
    pub images: HashMap<NodeId, Rc<Image>>,
    pub layout: Layout,
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Escape for use inside a quoted HTML attribute.
fn esc_attr(s: &str) -> String {
    esc(s).replace('"', "&quot;").replace('\'', "&#39;")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

fn without_fragment(u: &Url) -> Url {
    let mut u = u.clone();
    u.set_fragment(None);
    u
}

/// A short human name for a document: its decoded file name, else the host.
/// `data:` URLs (which can be megabytes long) just say so.
fn document_name(url: &Url) -> String {
    if url.scheme() == "data" {
        return "data document".into();
    }
    let segment = url.path_segments().and_then(|mut s| s.next_back()).filter(|s| !s.is_empty());
    let name = match segment {
        Some(s) => String::from_utf8_lossy(&net::percent_decode_bytes(s)).into_owned(),
        None => url.host_str().map(String::from).unwrap_or_else(|| url.as_str().to_string()),
    };
    truncate_chars(&name, MAX_TITLE_CHARS)
}

/// Does a `media` attribute apply to a screen? Feature-only queries like
/// `(min-width: 600px)` can't be evaluated yet and are assumed to match.
fn media_applies(media: Option<&str>) -> bool {
    let Some(m) = media else { return true };
    let m = m.trim().to_ascii_lowercase();
    m.is_empty()
        || m.split(',').any(|q| {
            let q = q.trim();
            q.starts_with("all") || q.starts_with("screen") || q.starts_with('(') || q.starts_with("not ")
        })
}

/// `rel="stylesheet"` (possibly among other tokens) but not `alternate stylesheet`.
fn is_stylesheet_link(rel: Option<&str>) -> bool {
    let Some(rel) = rel else { return false };
    let tokens: Vec<String> = rel.split_whitespace().map(|t| t.to_ascii_lowercase()).collect();
    tokens.iter().any(|t| t == "stylesheet") && !tokens.iter().any(|t| t == "alternate")
}

/// Fetch many URLs with a small pool of worker threads that pull from a shared
/// queue, so one slow server only ever stalls one worker. No new request is
/// started after `deadline`, and bodies stop being kept once `MAX_FETCHED_BYTES`
/// is reached. Results are keyed by the URL string requested; failures are absent.
fn fetch_parallel(urls: Vec<Url>, deadline: Duration) -> HashMap<String, Fetched> {
    let started = Instant::now();
    let next = AtomicUsize::new(0);
    let total = AtomicU64::new(0);
    let results = Mutex::new(HashMap::new());
    std::thread::scope(|s| {
        for _ in 0..PARALLEL_FETCHES.min(urls.len()) {
            s.spawn(|| loop {
                if started.elapsed() >= deadline {
                    break;
                }
                let Some(u) = urls.get(next.fetch_add(1, Ordering::SeqCst)) else { break };
                // a panic in one fetch must not take the whole page load down
                let Ok(Ok(f)) = std::panic::catch_unwind(|| net::fetch(u)) else { continue };
                let len = f.body.len() as u64;
                if total.fetch_add(len, Ordering::SeqCst) + len > MAX_FETCHED_BYTES {
                    continue;
                }
                if let Ok(mut map) = results.lock() {
                    map.insert(u.to_string(), f);
                }
            });
        }
    });
    results.into_inner().unwrap_or_default()
}

/// A simple spending limit, used for decoded image memory.
struct Budget {
    remaining: u64,
}

impl Budget {
    fn new(total: u64) -> Budget {
        Budget { remaining: total }
    }

    /// Spend `bytes` if there is enough left.
    fn take(&mut self, bytes: u64) -> bool {
        if bytes <= self.remaining {
            self.remaining -= bytes;
            true
        } else {
            false
        }
    }
}

enum Sheet {
    Inline(String),
    Link(Url),
}

impl Page {
    /// Fetch and build the page at `url`. Never fails: network and decoding
    /// errors become an error page.
    pub fn load(url: &Url) -> Page {
        match net::fetch(url) {
            Ok(f) => Page::from_fetched(f),
            Err(e) => Page::error(url, &e),
        }
    }

    pub fn error(url: &Url, message: &str) -> Page {
        Page::from_source(
            url,
            &format!(
                "<title>Error</title><h1>Could not open page</h1><p><code>{}</code></p><p>{}</p>",
                esc(&truncate_chars(url.as_str(), 300)),
                esc(&truncate_chars(message, 500))
            ),
        )
    }

    fn from_fetched(f: Fetched) -> Page {
        let name = document_name(&f.url);
        let mut page = match f.content_type.as_str() {
            "text/html" | "application/xhtml+xml" => {
                Page::from_source(&f.url, &net::decode_text(&f.body, f.charset.as_deref(), true))
            }
            "text/plain" | "text/css" | "text/javascript" | "application/javascript" | "application/json"
            | "text/xml" | "application/xml" | "text/markdown" => {
                let text = net::decode_text(&f.body, f.charset.as_deref(), false);
                Page::from_source(&f.url, &format!("<title>{}</title><pre>{}</pre>", esc(&name), esc(&text)))
            }
            c if c.starts_with("image/") => match images::decode(&f.body) {
                Some(img) => {
                    let title = format!("{name} ({}×{})", img.w, img.h);
                    let src = format!(
                        "<title>{}</title><body style='margin:0;background:#1e1e1e'><img src=\"{}\">",
                        esc(&title),
                        esc_attr(f.url.as_str())
                    );
                    let mut preloaded = HashMap::new();
                    preloaded.insert(without_fragment(&f.url).to_string(), Rc::new(img));
                    Page::build(&f.url, &src, preloaded)
                }
                None => Page::error(&f.url, &format!("Cannot decode this {} image (only PNG is supported so far)", f.content_type)),
            },
            other => Page::error(&f.url, &format!("Cannot display content of type '{other}' (downloads aren't supported yet)")),
        };
        page.status = Some(f.status);
        page
    }

    /// Build a page from markup already in memory. Relative URLs resolve against `url`.
    pub fn from_source(url: &Url, src: &str) -> Page {
        Page::build(url, src, HashMap::new())
    }

    /// A short label for tabs and the window title.
    pub fn display_title(&self) -> String {
        let title = self.title.trim();
        if !title.is_empty() {
            return truncate_chars(title, MAX_TITLE_CHARS);
        }
        document_name(&self.url)
    }

    fn build(url: &Url, src: &str, preloaded: HashMap<String, Rc<Image>>) -> Page {
        // Normalise Windows/old-Mac line endings so <pre> doesn't render stray '\r' glyphs.
        let src = src.replace("\r\n", "\n").replace('\r', "\n");
        let dom = html::parse(&src);

        // <base href> changes how every relative URL in the document resolves.
        let mut base = url.clone();
        for id in 0..dom.nodes.len() {
            if dom.tag(id) == Some("base") {
                if let Some(b) = dom.attr(id, "href").and_then(|h| url.join(h.trim()).ok()) {
                    base = b;
                }
                break;
            }
        }
        // An http(s) page must never be able to read local files.
        let allowed = |target: &Url| !(matches!(url.scheme(), "http" | "https") && target.scheme() == "file");
        // Resource URLs are compared without their `#fragment` (it never reaches the server).
        let resolve = |href: &str| base.join(href.trim()).ok().map(|u| without_fragment(&u)).filter(&allowed);

        // Pass 1: find stylesheets and images, and the URLs that need fetching.
        let mut sheets: Vec<Sheet> = vec![];
        let mut title = String::new();
        let mut to_fetch: Vec<Url> = vec![];
        let mut queued: HashSet<String> = HashSet::new();
        let mut image_urls: Vec<(NodeId, Url)> = vec![];
        for id in 0..dom.nodes.len() {
            let Kind::Element { tag, .. } = &dom.nodes[id].kind else { continue };
            match tag.as_str() {
                "style" if media_applies(dom.attr(id, "media")) => sheets.push(Sheet::Inline(dom.text_content(id))),
                "link" if is_stylesheet_link(dom.attr(id, "rel")) && media_applies(dom.attr(id, "media")) => {
                    if let Some(u) = dom.attr(id, "href").and_then(&resolve) {
                        if sheets.iter().filter(|s| matches!(s, Sheet::Link(_))).count() < MAX_STYLESHEETS {
                            if queued.insert(u.to_string()) {
                                to_fetch.push(u.clone());
                            }
                            sheets.push(Sheet::Link(u));
                        }
                    }
                }
                "title" if title.is_empty() => title = dom.text_content(id).trim().to_string(),
                "img" => {
                    if let Some(u) = dom.attr(id, "src").and_then(&resolve) {
                        if image_urls.len() < MAX_IMAGES {
                            if !preloaded.contains_key(u.as_str()) && queued.insert(u.to_string()) {
                                to_fetch.push(u.clone());
                            }
                            image_urls.push((id, u));
                        }
                    }
                }
                _ => {}
            }
        }

        // Pass 2: fetch everything at once.
        let mut fetched = fetch_parallel(to_fetch, SUBRESOURCE_DEADLINE);

        // Stylesheets in cascade order: user agent, then document order.
        let mut rules: Vec<Rule> = vec![];
        css::parse_stylesheet_origin(style::USER_AGENT_CSS, &mut rules, true);
        for sheet in &sheets {
            match sheet {
                Sheet::Inline(text) => css::parse_stylesheet(text, &mut rules),
                Sheet::Link(u) => {
                    if let Some(f) = fetched.get(u.as_str()).filter(|f| f.status < 400) {
                        css::parse_stylesheet(&net::decode_text(&f.body, f.charset.as_deref(), false), &mut rules);
                    }
                }
            }
        }
        let styles = style::compute(&dom, &rules);

        // Decode each distinct image once, share it between <img> elements, free
        // its raw bytes right away, and stop when the page's memory budget is spent.
        let mut budget = Budget::new(IMAGE_BUDGET);
        let mut decoded: HashMap<String, Option<Rc<Image>>> = HashMap::new();
        let mut images = HashMap::new();
        for (id, u) in image_urls {
            let key = u.to_string();
            if !decoded.contains_key(&key) {
                let img = preloaded.get(&key).cloned().or_else(|| {
                    let f = fetched.remove(&key).filter(|f| f.status < 400)?;
                    let img = images::decode(&f.body)?;
                    budget.take(img.bytes()).then(|| Rc::new(img))
                });
                decoded.insert(key.clone(), img);
            }
            if let Some(Some(img)) = decoded.get(&key) {
                images.insert(id, img.clone());
            }
        }

        let layout = Layout { items: vec![], links: vec![], height: 0.0, canvas_bg: None };
        Page { url: url.clone(), status: None, title, dom, styles, images, layout }
    }

    /// Re-run layout for a new viewport width (logical pixels).
    pub fn relayout(&mut self, fonts: &Fonts, width: f32) {
        self.layout = layout::layout_document(&self.dom, &self.styles, fonts, &self.images, width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::Item;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};

    fn t_url() -> Url {
        Url::parse("file:///tmp/t.html").unwrap()
    }

    fn url_of(path: &Path) -> Url {
        Url::from_file_path(path).unwrap()
    }

    fn render(src: &str, width: f32) -> Page {
        let fonts = Fonts::load();
        let mut p = Page::from_source(&t_url(), src);
        p.relayout(&fonts, width);
        p
    }

    fn rects(p: &Page) -> Vec<(f32, f32, f32, f32)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Rect { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
            _ => None,
        }).collect()
    }

    #[test]
    fn sibling_margins_collapse() {
        // 10 + 20 + max(10,10) + 20 + 10 = 70 (not 90)
        let p = render("<style>body{margin:0}</style><div style='height:20px;margin:10px 0'></div><div style='height:20px;margin:10px 0'></div>", 400.0);
        assert_eq!(p.layout.height, 70.0);
    }

    #[test]
    fn negative_margins_collapse_by_summing_mixed_signs() {
        let p = render("<style>body{margin:0}</style><div style='height:20px;margin-bottom:30px'></div><div style='height:20px;margin-top:-10px'></div>", 400.0);
        // 20 + (30 + -10) + 20 = 60
        assert_eq!(p.layout.height, 60.0);
    }

    #[test]
    fn parent_and_first_child_top_margins_collapse() {
        let p = render("<style>body{margin:0}</style><div style='margin-top:8px'><div style='margin-top:20px;height:10px;background:red'></div></div>", 400.0);
        let red = rects(&p)[0];
        assert_eq!(red.1, 20.0, "child should start at max(8,20), not 8+20");
    }

    #[test]
    fn padding_or_border_stops_margin_collapsing() {
        let p = render("<style>body{margin:0}</style><div style='margin-top:8px;padding-top:1px'><div style='margin-top:20px;height:10px;background:red'></div></div>", 400.0);
        assert_eq!(rects(&p)[0].1, 8.0 + 1.0 + 20.0);
    }

    #[test]
    fn border_box_width_includes_padding() {
        let p = render("<style>body{margin:0}</style><div style='width:200px;padding:10px;box-sizing:border-box;background:red'>x</div>", 400.0);
        assert_eq!(rects(&p)[0].2, 200.0);
        let p = render("<style>body{margin:0}</style><div style='width:200px;padding:10px;background:red'>x</div>", 400.0);
        assert_eq!(rects(&p)[0].2, 220.0);
    }

    #[test]
    fn max_width_with_auto_margins_centres() {
        let p = render("<style>body{margin:0}</style><div style='max-width:100px;margin:0 auto;background:red'>x</div>", 400.0);
        let r = rects(&p)[0];
        assert_eq!((r.0, r.2), (150.0, 100.0));
    }

    #[test]
    fn text_wraps_in_narrow_viewport() {
        let src = "<p>the quick brown fox jumps over the lazy dog</p>";
        let lines = |w| {
            let p = render(src, w);
            let mut ys: Vec<i32> = p.layout.items.iter().filter_map(|i| match i {
                Item::Text { baseline, .. } => Some(*baseline as i32), _ => None }).collect();
            ys.dedup();
            ys.len()
        };
        assert_eq!(lines(2000.0), 1);
        let (mid, narrow) = (lines(120.0), lines(60.0));
        assert!(mid >= 3 && narrow > mid, "{mid} {narrow}");
    }

    #[test]
    fn negative_and_huge_sizes_do_not_panic_through_paint() {
        use crate::paint::Painter;
        let fonts = Fonts::load();
        let src = "<div style='width:-50px;height:-20px;padding:-5px;margin:-9999px;border:2px solid red;background:red'>x</div>\
                   <p style='font-size:99999999px'>y</p><div style='width:1px'>unbreakable_word_much_wider_than_container</div>";
        let mut p = Page::from_source(&t_url(), src);
        p.relayout(&fonts, 300.0);
        let (w, h) = (300usize, 200usize);
        Painter::default().paint(&mut vec![0; w * h], w, h, &p.layout, &fonts, 0.0, 2.0 / 2.0);
    }

    #[test]
    fn display_none_and_whitespace_between_blocks_add_no_height() {
        let p = render("<style>body{margin:0}</style><div style='height:10px'></div>\n  \n<span style='display:none'>hidden</span>\n<div style='height:10px'></div>", 400.0);
        assert_eq!(p.layout.height, 20.0);
    }

    #[test]
    fn pre_preserves_lines_and_spaces() {
        let p = render("<style>body{margin:0}pre{margin:0}</style><pre>a\n  b\n\nc</pre>", 400.0);
        let n = p.layout.items.iter().filter(|i| matches!(i, Item::Text { .. })).count();
        assert_eq!(n, 3); // "a", "  b", "c"; the blank line is just vertical space
    }

    // ---------- images ----------

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = vec![];
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let px: Vec<u8> = (0..w * h).flat_map(|_| [200u8, 30, 30, 255]).collect();
        enc.write_header().unwrap().write_image_data(&px).unwrap();
        out
    }

    /// A fresh temp dir containing `a.png` (solid red, w x h).
    fn temp_dir_with_png(name: &str, w: u32, h: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mybrowser-test-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), png_bytes(w, h)).unwrap();
        dir
    }

    fn render_in(dir: &Path, body: &str, width: f32) -> Page {
        let fonts = Fonts::load();
        let src = format!("<style>body{{margin:0}}</style>{body}");
        let mut p = Page::from_source(&url_of(&dir.join("t.html")), &src);
        p.relayout(&fonts, width);
        p
    }

    fn images_of(p: &Page) -> Vec<(f32, f32, f32, f32)> {
        p.layout.items.iter().filter_map(|i| match i {
            Item::Image { x, y, w, h, .. } => Some((*x, *y, *w, *h)),
            _ => None,
        }).collect()
    }

    fn base64_encode(data: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for c in data.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            out.push(T[(n >> 18) as usize & 63] as char);
            out.push(T[(n >> 12) as usize & 63] as char);
            out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[test]
    fn image_uses_natural_size_and_attribute_aspect_ratio() {
        let dir = temp_dir_with_png("size", 40, 20);
        let p = render_in(&dir, "<img src=a.png>", 400.0);
        let im = images_of(&p);
        assert_eq!((im[0].2, im[0].3), (40.0, 20.0));
        let p = render_in(&dir, "<img src=a.png width=80>", 400.0);
        assert_eq!((images_of(&p)[0].2, images_of(&p)[0].3), (80.0, 40.0)); // h keeps the 2:1 ratio
        let p = render_in(&dir, "<img src=a.png height=10>", 400.0);
        assert_eq!((images_of(&p)[0].2, images_of(&p)[0].3), (20.0, 10.0));
        // CSS beats attributes
        let p = render_in(&dir, "<img src=a.png width=80 style='width:10px;height:10px'>", 400.0);
        assert_eq!((images_of(&p)[0].2, images_of(&p)[0].3), (10.0, 10.0));
    }

    #[test]
    fn image_sits_on_the_text_baseline() {
        let dir = temp_dir_with_png("baseline", 30, 30);
        let p = render_in(&dir, "<p style='margin:0'>hello <img src=a.png> world</p>", 400.0);
        let (_, y, _, h) = images_of(&p)[0];
        let baselines: Vec<f32> = p.layout.items.iter().filter_map(|i| match i {
            Item::Text { baseline, .. } => Some(*baseline), _ => None }).collect();
        assert!(baselines.iter().all(|b| (b - (y + h)).abs() < 0.01), "{baselines:?} vs bottom {}", y + h);
    }

    #[test]
    fn missing_or_corrupt_image_falls_back_to_alt_text() {
        let dir = temp_dir_with_png("bad", 4, 4);
        std::fs::write(dir.join("junk.png"), b"this is not a png").unwrap();
        for src in ["nope.png", "junk.png", "data:image/png;base64,AAAA", "ftp://example.com/x.png"] {
            let p = render_in(&dir, &format!("<img src='{src}' alt='fallback'>"), 400.0);
            assert!(images_of(&p).is_empty(), "{src}");
            assert!(p.layout.items.iter().any(|i| matches!(i, Item::Text { text, .. } if text == "[fallback]")), "{src}");
        }
    }

    #[test]
    fn image_inside_link_is_clickable() {
        let dir = temp_dir_with_png("link", 30, 10);
        let p = render_in(&dir, "<a href='next.html'><img src=a.png></a>", 400.0);
        let l = &p.layout.links[0];
        assert_eq!((l.href.as_str(), l.w, l.h), ("next.html", 30.0, 10.0));
    }

    #[test]
    fn huge_or_offscreen_images_paint_without_panicking() {
        use crate::paint::Painter;
        let dir = temp_dir_with_png("huge", 8, 8);
        let fonts = Fonts::load();
        let p = render_in(&dir, "<img src=a.png width=1000000 height=1000000><img src=a.png width=0 height=0>", 300.0);
        let (w, h) = (120usize, 90usize);
        for scale in [1.0, 2.0] {
            let mut buf = vec![0u32; w * h];
            Painter::default().paint(&mut buf, w, h, &p.layout, &fonts, 5000.0, scale);
            Painter::default().paint(&mut buf, w, h, &p.layout, &fonts, 0.0, scale);
        }
        let mut buf = vec![0u32; w * h];
        Painter::default().paint(&mut buf, w, h, &p.layout, &fonts, 0.0, 1.0);
        assert_eq!(buf[10 * w + 10], 0xC81E1E, "image pixels should be painted red");
    }

    #[test]
    fn data_url_images_load() {
        let dir = temp_dir_with_png("data", 4, 4);
        let uri = format!("data:image/png;base64,{}", base64_encode(&png_bytes(6, 3)));
        let p = render_in(&dir, &format!("<img src='{uri}'>"), 400.0);
        let im = images_of(&p);
        assert_eq!((im[0].2, im[0].3), (6.0, 3.0));
    }

    #[test]
    fn the_same_image_with_different_fragments_is_one_resource() {
        let dir = temp_dir_with_png("frag", 5, 5);
        let p = render_in(&dir, "<img src='a.png#1'><img src='a.png#2'><img src='a.png'>", 400.0);
        assert_eq!(images_of(&p).len(), 3);
        // all three share one decoded copy
        let mut ptrs: Vec<_> = p.images.values().map(Rc::as_ptr).collect();
        ptrs.dedup();
        assert_eq!(p.images.len(), 3);
        assert!(ptrs.windows(2).all(|w| w[0] == w[1]) || ptrs.len() == 1);
    }

    // ---------- stylesheets, base URLs, security ----------

    #[test]
    fn linked_stylesheets_resolve_relative_to_the_page_and_honour_rel_and_media() {
        let dir = temp_dir_with_png("css", 4, 4);
        std::fs::write(dir.join("s.css"), "div { background: red }").unwrap();
        let link = |attrs: &str| format!("<link {attrs}><div style='height:5px'></div>");
        let red = |body: &str| rects(&render_in(&dir, body, 200.0)).len();
        assert_eq!(red(&link("rel=stylesheet href=s.css")), 1);
        assert_eq!(red(&link("rel='preload stylesheet' href=s.css")), 1);
        assert_eq!(red(&link("rel='alternate stylesheet' href=s.css")), 0, "alternate sheets are off by default");
        assert_eq!(red(&link("rel=stylesheet href=s.css media=print")), 0);
        assert_eq!(red(&link("rel=stylesheet href=s.css media='screen and (min-width: 1px)'")), 1);
        assert_eq!(red(&link("rel=stylesheet href=missing.css")), 0);
        assert_eq!(red(&link("rel=icon href=s.css")), 0);
    }

    #[test]
    fn base_href_changes_resolution() {
        let dir = temp_dir_with_png("base", 4, 4);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.png"), png_bytes(9, 9)).unwrap();
        let p = render_in(&dir, "<base href='sub/'><img src=b.png>", 200.0);
        assert_eq!(images_of(&p).len(), 1);
        let p = render_in(&dir, "<img src=b.png>", 200.0);
        assert!(images_of(&p).is_empty());
    }

    #[test]
    fn network_pages_cannot_read_local_files() {
        let dir = temp_dir_with_png("sec", 4, 4);
        let secret = url_of(&dir.join("a.png"));
        // No network is touched: the file: URL is rejected before any fetch.
        let src = format!("<link rel=stylesheet href='{secret}'><img src='{secret}' alt='blocked'>");
        let p = Page::from_source(&Url::parse("http://example.invalid/").unwrap(), &src);
        assert!(p.images.is_empty());
    }

    #[test]
    fn error_pages_escape_their_text() {
        let p = Page::error(&Url::parse("http://x.test/<script>").unwrap(), "<img src=x onerror=1> & more");
        let text = p.dom.text_content(0);
        assert!(text.contains("<img src=x onerror=1> & more"), "{text}");
        assert!((0..p.dom.nodes.len()).all(|i| p.dom.tag(i) != Some("img") && p.dom.tag(i) != Some("script")));
    }

    // ---------- loading documents of different types ----------

    #[test]
    fn plain_text_and_image_documents_get_a_page() {
        let dir = temp_dir_with_png("types", 12, 7);
        std::fs::write(dir.join("n.txt"), "a <b> & c").unwrap();
        let p = Page::load(&url_of(&dir.join("n.txt")));
        assert!(p.dom.text_content(0).contains("a <b> & c"));
        assert_eq!(p.title, "n.txt");
        let fonts = Fonts::load();
        let mut p = Page::load(&url_of(&dir.join("a.png")));
        p.relayout(&fonts, 300.0);
        assert_eq!(images_of(&p).len(), 1);
        assert_eq!(p.title, "a.png (12×7)");
        // unsupported types and missing files become readable error pages
        std::fs::write(dir.join("blob.bin"), [0u8, 159, 146, 150]).unwrap();
        assert!(Page::load(&url_of(&dir.join("blob.bin"))).title == "Error");
        assert!(Page::load(&url_of(&dir.join("nope.html"))).title == "Error");
    }

    #[test]
    fn document_names_are_short_decoded_and_safe() {
        assert_eq!(document_name(&Url::parse("file:///tmp/my%20file.txt").unwrap()), "my file.txt");
        assert_eq!(document_name(&Url::parse("https://example.org/").unwrap()), "example.org");
        assert_eq!(document_name(&Url::parse("file:///").unwrap()), "file:///");
        let big = format!("data:text/plain,{}", "x".repeat(100_000));
        assert_eq!(document_name(&Url::parse(&big).unwrap()), "data document");
        let long = format!("https://example.org/{}", "y".repeat(500));
        assert!(document_name(&Url::parse(&long).unwrap()).chars().count() <= MAX_TITLE_CHARS + 1);
        // a data: URL document gets a short title too
        let p = Page::load(&Url::parse(&big).unwrap());
        assert_eq!(p.title, "data document");
        // and a very long <title> is clipped for the window
        let p = Page::from_source(&t_url(), &format!("<title>{}</title>", "z".repeat(5000)));
        assert!(p.display_title().chars().count() <= MAX_TITLE_CHARS + 1);
    }

    /// A tiny multi-route HTTP server: serves `routes` (path -> (content type, body))
    /// on a free port, handling each connection on its own thread, `n` requests in total.
    fn serve_routes(routes: Vec<(&'static str, &'static str, Vec<u8>)>, n: usize) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let routes = std::sync::Arc::new(routes);
            let mut workers = vec![];
            for _ in 0..n {
                let (mut sock, _) = listener.accept().unwrap();
                let routes = routes.clone();
                workers.push(std::thread::spawn(move || {
                    let mut seen = vec![];
                    let mut buf = [0u8; 2048];
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        let k = sock.read(&mut buf).unwrap_or(0);
                        if k == 0 {
                            return;
                        }
                        seen.extend_from_slice(&buf[..k]);
                    }
                    let line = String::from_utf8_lossy(&seen).lines().next().unwrap_or("").to_string();
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, ctype, body) = match routes.iter().find(|(p, _, _)| *p == path) {
                        Some((_, c, b)) => ("200 OK", *c, b.clone()),
                        None => ("404 Not Found", "text/plain", b"nope".to_vec()),
                    };
                    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(&body);
                }));
            }
            for w in workers {
                let _ = w.join();
            }
        });
        (port, handle)
    }

    #[test]
    fn a_page_over_http_loads_its_stylesheet_and_image_from_the_network() {
        let html = "<!doctype html><title>Net page</title><link rel=stylesheet href=/s.css><body style='margin:0'><div id=a></div><img src='/pic.png'>";
        let (port, server) = serve_routes(
            vec![
                ("/index.html", "text/html; charset=utf-8", html.as_bytes().to_vec()),
                ("/s.css", "text/css", b"#a { height: 11px; background: red }".to_vec()),
                ("/pic.png", "image/png", png_bytes(20, 10)),
            ],
            3,
        );
        let fonts = Fonts::load();
        let mut p = Page::load(&Url::parse(&format!("http://127.0.0.1:{port}/index.html")).unwrap());
        server.join().unwrap();
        p.relayout(&fonts, 300.0);
        assert_eq!((p.title.as_str(), p.status), ("Net page", Some(200)));
        assert_eq!(rects(&p)[0].3, 11.0, "stylesheet from the network applied");
        assert_eq!(images_of(&p)[0].2, 20.0, "image from the network decoded");
    }

    #[test]
    fn http_error_pages_render_and_report_their_status() {
        let (port, server) = serve_routes(vec![], 1);
        let p = Page::load(&Url::parse(&format!("http://127.0.0.1:{port}/gone")).unwrap());
        server.join().unwrap();
        assert_eq!(p.status, Some(404));
        assert!(p.dom.text_content(0).contains("nope"));
    }

    #[test]
    fn image_documents_with_ampersands_in_the_url_are_fetched_only_once() {
        // The URL goes through an HTML attribute; unescaped, `&lt=2` was decoded to `<=2`,
        // the preloaded image no longer matched, and the image was downloaded again.
        let (port, server) = serve_routes(vec![("/a.png?x=1&lt=2", "image/png", png_bytes(4, 4))], 1);
        let fonts = Fonts::load();
        let mut p = Page::load(&Url::parse(&format!("http://127.0.0.1:{port}/a.png?x=1&lt=2")).unwrap());
        server.join().unwrap(); // a second request would have no server left to answer it
        p.relayout(&fonts, 200.0);
        assert_eq!(images_of(&p).len(), 1);
    }

    #[test]
    fn fragments_do_not_cause_duplicate_downloads() {
        let html = "<img src='/p.png#1'><img src='/p.png#2'>";
        let (port, server) = serve_routes(
            vec![("/i.html", "text/html", html.as_bytes().to_vec()), ("/p.png", "image/png", png_bytes(3, 3))],
            2, // the page and ONE image request
        );
        let p = Page::load(&Url::parse(&format!("http://127.0.0.1:{port}/i.html")).unwrap());
        server.join().unwrap();
        assert_eq!(p.images.len(), 2, "both <img> elements got the image");
    }

    #[test]
    fn parallel_fetching_is_bounded_and_survives_failures() {
        // many unreachable URLs finish quickly and yield nothing
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let urls: Vec<Url> = (0..60).map(|i| Url::parse(&format!("http://127.0.0.1:{dead}/{i}")).unwrap()).collect();
        let t = Instant::now();
        assert!(fetch_parallel(urls.clone(), Duration::from_secs(30)).is_empty());
        assert!(t.elapsed().as_secs() < 10);
        // a deadline of zero starts no requests at all
        let (port, server) = serve_routes(vec![("/x", "text/plain", b"hi".to_vec())], 0);
        let u = Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap();
        assert!(fetch_parallel(vec![u; 5], Duration::ZERO).is_empty());
        server.join().unwrap();
        // results from a live server are keyed by the requested URL
        let (port, server) = serve_routes(vec![("/a", "text/plain", b"A".to_vec()), ("/b", "text/plain", b"B".to_vec())], 2);
        let (a, b) = (Url::parse(&format!("http://127.0.0.1:{port}/a")).unwrap(), Url::parse(&format!("http://127.0.0.1:{port}/b")).unwrap());
        let got = fetch_parallel(vec![a.clone(), b.clone()], Duration::from_secs(30));
        server.join().unwrap();
        assert_eq!(got[a.as_str()].body, b"A");
        assert_eq!(got[b.as_str()].body, b"B");
    }

    #[test]
    fn the_image_memory_budget_is_enforced() {
        let mut b = Budget::new(100);
        assert!(b.take(60));
        assert!(!b.take(41), "would exceed the budget");
        assert!(b.take(40));
        assert!(!b.take(1));
        assert!(Budget::new(0).take(0));
    }

    #[test]
    fn display_title_falls_back_to_host_or_file_name() {
        let p = Page::from_source(&Url::parse("https://example.org/a/b.html").unwrap(), "<p>x</p>");
        assert_eq!(p.display_title(), "b.html");
        let p = Page::from_source(&Url::parse("https://example.org/").unwrap(), "<p>x</p>");
        assert_eq!(p.display_title(), "example.org");
        let p = Page::from_source(&Url::parse("file:///tmp/notes.html").unwrap(), "<p>x</p>");
        assert_eq!(p.display_title(), "notes.html");
        let p = Page::from_source(&t_url(), "<title>Hi</title>");
        assert_eq!(p.display_title(), "Hi");
    }
}
