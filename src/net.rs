//! Resource loading for `file:`, `http:`, `https:` and `data:` URLs.
//!
//! All network requests go through one shared agent and the process-wide cookie
//! jar in `cookies.rs`. Redirects are followed here, by hand, so that cookies set
//! by a redirecting response are stored and sent on the next hop, and so that a
//! redirect can never lead anywhere but http(s).
use crate::cookies;
use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

/// Largest response body we will read (pages, stylesheets, images).
pub const MAX_BODY: u64 = 32 * 1024 * 1024;

/// Largest form body we will send.
pub const MAX_POST_BODY: usize = 8 * 1024 * 1024;

/// Redirects followed before giving up.
const MAX_REDIRECTS: usize = 10;

pub struct Fetched {
    /// The final URL, after any redirects.
    pub url: Url,
    pub status: u16,
    /// Lower-case MIME type without parameters, sniffed if the server gave none.
    pub content_type: String,
    pub charset: Option<String>,
    pub body: Vec<u8>,
}

const ACCEPT: &str = "text/html,application/xhtml+xml,text/css,image/png,*/*;q=0.8";

/// Load a top-level document (what the user navigated to).
pub fn fetch(url: &Url) -> Result<Fetched, String> {
    fetch_in(url, None, None)
}

/// Submit a form: POST `body` (already `application/x-www-form-urlencoded`) to
/// `url`. Only http(s) can be posted to. A redirect after the POST is followed
/// with a plain GET (as browsers do for 301, 302 and 303).
pub fn fetch_post(url: &Url, body: &str) -> Result<Fetched, String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("can't submit a form to {}: URLs", url.scheme()));
    }
    if body.len() > MAX_POST_BODY {
        return Err("form data is too large to send".into());
    }
    fetch_in(url, None, Some(body))
}

/// Load a subresource (stylesheet, image) of the page at `top`. Where it comes
/// from a different site than the page, no cookies are sent or stored: a
/// tracking pixel on another domain can't recognise you.
pub fn fetch_subresource(url: &Url, top: &Url) -> Result<Fetched, String> {
    fetch_in(url, Some(top), None)
}

fn fetch_in(url: &Url, top: Option<&Url>, post: Option<&str>) -> Result<Fetched, String> {
    match url.scheme() {
        "file" => fetch_file(url),
        "http" | "https" => fetch_http(url, top, post),
        "data" => fetch_data(url),
        other => Err(format!("unsupported URL scheme '{other}:'")),
    }
}

fn fetch_file(url: &Url) -> Result<Fetched, String> {
    let path = url.to_file_path().map_err(|_| format!("not a local file path: {url}"))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Devices and FIFOs (`/dev/zero`, `/dev/urandom`) report a length of 0 but
    // never end, so only regular files are documents.
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    // Bound by what is actually read, not by the size the file claimed a moment
    // ago: it may grow in between.
    let mut body = Vec::new();
    std::fs::File::open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .take(MAX_BODY + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if body.len() as u64 > MAX_BODY {
        return Err(format!("{} is too large", path.display()));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let content_type = match ext.as_str() {
        "html" | "htm" | "xhtml" => "text/html",
        "css" => "text/css",
        "txt" | "md" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "js" => "text/javascript",
        _ => sniff(&body),
    };
    Ok(Fetched { url: url.clone(), status: 200, content_type: content_type.into(), charset: None, body })
}

fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(20)))
            // Redirects are followed by `fetch_http` (see the module docs).
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent("Mozilla/5.0 (compatible; mybrowser/0.1)")
            // An HTTP error status still has a body worth showing (404 pages etc.).
            .http_status_as_error(false)
            .build()
            .into()
    })
}

fn fetch_http(start: &Url, top: Option<&Url>, post: Option<&str>) -> Result<Fetched, String> {
    let mut url = start.clone();
    let mut post = post;
    for _ in 0..=MAX_REDIRECTS {
        // Each hop is judged on its own: a redirect to another site loses the cookies.
        let use_cookies = top.is_none_or(|t| cookies::same_site(t, &url));
        let cookie_header = if use_cookies { cookies::global().header_for(&url) } else { None };
        let result = match post {
            Some(body) => {
                let mut req = agent()
                    .post(url.as_str())
                    .header("Accept", ACCEPT)
                    .header("Accept-Language", "en")
                    .header("Content-Type", "application/x-www-form-urlencoded");
                if let Some(c) = cookie_header {
                    req = req.header("Cookie", c);
                }
                req.send(body)
            }
            None => {
                let mut req = agent().get(url.as_str()).header("Accept", ACCEPT).header("Accept-Language", "en");
                if let Some(c) = cookie_header {
                    req = req.header("Cookie", c);
                }
                req.call()
            }
        };
        let mut resp = result.map_err(|e| format!("{url}: {e}"))?;
        if use_cookies {
            let sets: Vec<String> =
                resp.headers().get_all("set-cookie").into_iter().filter_map(|v| v.to_str().ok()).map(String::from).collect();
            if !sets.is_empty() {
                let mut jar = cookies::global();
                for s in &sets {
                    jar.accept(&url, s);
                }
            }
        }
        let status = resp.status();
        if status.is_redirection() {
            let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(String::from);
            if let Some(loc) = location {
                let next = url.join(loc.trim()).map_err(|e| format!("{url}: bad redirect '{loc}': {e}"))?;
                if !matches!(next.scheme(), "http" | "https") {
                    return Err(format!("{url}: refusing to follow a redirect to {}:", next.scheme()));
                }
                url = next;
                post = None; // the redirected request is a GET
                continue;
            }
        }
        let content_type = resp.body().mime_type().unwrap_or("").to_ascii_lowercase();
        let charset = resp.body().charset().map(|c| c.to_ascii_lowercase());
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY)
            .read_to_vec()
            .map_err(|e| format!("reading {url}: {e}"))?;
        let content_type = if content_type.is_empty() { sniff(&body).to_string() } else { content_type };
        return Ok(Fetched { url, status: status.as_u16(), content_type, charset, body });
    }
    Err(format!("{start}: too many redirects"))
}

fn fetch_data(url: &Url) -> Result<Fetched, String> {
    // A data: URL's fragment is not part of its data.
    let mut url = url.clone();
    url.set_fragment(None);
    let rest = url.as_str().strip_prefix("data:").ok_or("not a data: URL")?;
    let (meta, data) = rest.split_once(',').ok_or("malformed data: URL (no comma)")?;
    let lower = meta.to_ascii_lowercase();
    let (meta, is_base64) = match lower.strip_suffix(";base64") {
        Some(m) => (m.to_string(), true),
        None => (lower, false),
    };
    let mut params = meta.split(';');
    let mime = params.next().filter(|m| !m.is_empty()).unwrap_or("text/plain").to_string();
    let charset = params.find_map(|p| p.trim().strip_prefix("charset=")).map(String::from);
    let raw = percent_decode_bytes(data);
    let body = if is_base64 { base64_decode(&raw).ok_or("invalid base64 in data: URL")? } else { raw };
    if body.len() as u64 > MAX_BODY {
        return Err("data: URL too large".into());
    }
    Ok(Fetched { url, status: 200, content_type: mime, charset, body })
}

/// Guess a content type from the bytes themselves.
pub fn sniff(body: &[u8]) -> &'static str {
    if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return "image/jpeg";
    }
    if body.starts_with(b"GIF8") {
        return "image/gif";
    }
    if body.starts_with(&[0xFF, 0xFE]) || body.starts_with(&[0xFE, 0xFF]) {
        return "text/plain"; // UTF-16 text
    }
    let head = &body[..body.len().min(4096)];
    if head.contains(&0) {
        return "application/octet-stream";
    }
    // `<` only starts markup when a tag name, `!`, `?` or `/` follows (not `<3 love`).
    let mut non_space = head.iter().skip_while(|b| b.is_ascii_whitespace());
    if non_space.next() == Some(&b'<') && non_space.next().is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'!' | b'?' | b'/')) {
        return "text/html";
    }
    // valid UTF-8, allowing for a multi-byte character cut off at the sniff boundary
    let text_like = match std::str::from_utf8(head) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    };
    if text_like { "text/plain" } else { "application/octet-stream" }
}

pub fn percent_decode_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Standard and URL-safe base64. Whitespace is ignored; padding may only be
/// followed by more padding or whitespace.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    let mut padding = false;
    for &c in input {
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None; // data after the padding
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Turn response bytes into text. A BOM wins, then the HTTP charset, then (for
/// HTML only) a `<meta charset>` in the head. UTF-8 is the default; UTF-16,
/// Latin-1 and Windows-1252 are decoded properly, and anything else falls back
/// to lossy UTF-8.
pub fn decode_text(body: &[u8], header_charset: Option<&str>, is_html: bool) -> String {
    if let Some(rest) = body.strip_prefix(&[0xFF, 0xFE][..]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    if let Some(rest) = body.strip_prefix(&[0xFE, 0xFF][..]) {
        let units: Vec<u16> = rest.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return String::from_utf16_lossy(&units);
    }
    let (body, bom) = match body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let charset = if bom {
        None
    } else {
        header_charset.map(String::from).or_else(|| if is_html { meta_charset(body) } else { None })
    };
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "windows-1252" | "cp1252" | "us-ascii" | "ascii") => {
            body.iter().map(|&b| windows_1252(b)).collect()
        }
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

/// The charset declared by a real `<meta>` tag in the head. Only `<meta ...>`
/// tags before `</head>` count, so a `charset=` inside a script, a comment or
/// the page text is ignored.
fn meta_charset(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]).to_ascii_lowercase();
    let head = text.split("</head").next().unwrap_or(&text);
    let mut rest = head;
    while let Some(at) = rest.find("<meta") {
        let tag = &rest[at..];
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        if let Some(c) = tag.find("charset=") {
            let value: String = tag[c + "charset=".len()..]
                .trim_start_matches(['"', '\''])
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if !value.is_empty() {
                return Some(value);
            }
        }
        rest = &rest[at + "<meta".len()..];
    }
    None
}

fn windows_1252(b: u8) -> char {
    match b {
        0x80 => '€', 0x82 => '‚', 0x83 => 'ƒ', 0x84 => '„', 0x85 => '…', 0x86 => '†',
        0x87 => '‡', 0x88 => 'ˆ', 0x89 => '‰', 0x8A => 'Š', 0x8B => '‹', 0x8C => 'Œ',
        0x8E => 'Ž', 0x91 => '‘', 0x92 => '’', 0x93 => '“', 0x94 => '”', 0x95 => '•',
        0x96 => '–', 0x97 => '—', 0x98 => '˜', 0x99 => '™', 0x9A => 'š', 0x9B => '›',
        0x9C => 'œ', 0x9E => 'ž', 0x9F => 'Ÿ',
        b => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// Serve one canned response per connection on a free local port.
    fn serve(responses: Vec<String>) -> (u16, std::thread::JoinHandle<()>) {
        let (port, _heads, handle) = serve_recording(responses);
        (port, handle)
    }

    /// Like `serve`, but also records each request (lower-cased, head and any
    /// body that arrived with it), so tests can check what the browser sent.
    /// It reads the whole body when a `Content-Length` is given.
    fn serve_recording(responses: Vec<String>) -> (u16, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let seen_heads = heads.clone();
        let handle = std::thread::spawn(move || {
            for resp in responses {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let mut seen = vec![];
                let head_end = loop {
                    if let Some(i) = seen.windows(4).position(|w| w == b"\r\n\r\n") {
                        break Some(i + 4);
                    }
                    let n = sock.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break None;
                    }
                    seen.extend_from_slice(&buf[..n]);
                };
                if let Some(end) = head_end {
                    let head = String::from_utf8_lossy(&seen[..end]).to_ascii_lowercase();
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    while seen.len() < end + want {
                        let n = sock.read(&mut buf).unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        seen.extend_from_slice(&buf[..n]);
                    }
                }
                seen_heads.lock().unwrap().push(String::from_utf8_lossy(&seen).to_ascii_lowercase());
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        (port, heads, handle)
    }

    fn http(status: &str, headers: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len())
    }

    #[test]
    fn http_ok_returns_body_type_and_charset() {
        let (port, h) = serve(vec![http("200 OK", "Content-Type: text/html; charset=ISO-8859-1\r\n", "<p>hi</p>")]);
        let f = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/x")).unwrap()).unwrap();
        h.join().unwrap();
        assert_eq!((f.status, f.content_type.as_str(), f.charset.as_deref()), (200, "text/html", Some("iso-8859-1")));
        assert_eq!(f.body, b"<p>hi</p>");
    }

    #[test]
    fn http_redirects_are_followed_and_final_url_reported() {
        let (port, h) = serve(vec![
            http("302 Found", "Location: /two\r\n", ""),
            http("301 Moved", "Location: /three?q=1\r\n", ""),
            http("200 OK", "Content-Type: text/plain\r\n", "done"),
        ]);
        let f = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/one")).unwrap()).unwrap();
        h.join().unwrap();
        assert_eq!(f.url.path(), "/three");
        assert_eq!(f.url.query(), Some("q=1"));
        assert_eq!(f.body, b"done");
    }

    #[test]
    fn a_redirect_loop_gives_up_instead_of_spinning() {
        let loops: Vec<String> = (0..=MAX_REDIRECTS).map(|_| http("302 Found", "Location: /again\r\n", "")).collect();
        let (port, h) = serve(loops);
        let r = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/again")).unwrap());
        h.join().unwrap();
        assert!(r.err().is_some_and(|e| e.contains("too many redirects")));
    }

    #[test]
    fn a_redirect_status_without_a_location_is_just_a_response() {
        let (port, h) = serve(vec![http("302 Found", "Content-Type: text/plain\r\n", "stay here")]);
        let f = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()).unwrap();
        h.join().unwrap();
        assert_eq!((f.status, f.body.as_slice()), (302, &b"stay here"[..]));
    }

    #[test]
    fn http_error_statuses_still_deliver_their_body() {
        let (port, h) = serve(vec![http("404 Not Found", "Content-Type: text/html\r\n", "<h1>nope</h1>")]);
        let f = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/missing")).unwrap()).unwrap();
        h.join().unwrap();
        assert_eq!(f.status, 404);
        assert_eq!(f.body, b"<h1>nope</h1>");
    }

    #[test]
    fn redirects_to_local_files_are_not_followed_into_file_urls() {
        for target in ["file:///etc/hosts", "data:text/plain,hi", "ftp://example.com/x"] {
            let (port, h) = serve(vec![http("302 Found", &format!("Location: {target}\r\n"), "")]);
            let r = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap());
            h.join().unwrap();
            assert!(r.is_err(), "http must never redirect into {target}");
        }
    }

    #[test]
    fn connection_refused_and_unsupported_schemes_are_errors_not_panics() {
        // bind then drop to get a port nobody is listening on
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        assert!(fetch(&Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()).is_err());
        assert!(fetch(&Url::parse("ftp://example.com/x").unwrap()).is_err());
        assert!(fetch(&Url::parse("mailto:a@b.c").unwrap()).is_err());
        assert!(fetch(&Url::parse("file:///definitely/not/here.html").unwrap()).is_err());
    }

    // ---------- POST ----------

    #[test]
    fn a_form_post_sends_its_body_content_type_and_cookies() {
        let (port, heads, h) = serve_recording(vec![
            http("200 OK", "Set-Cookie: post_ck=1; Path=/\r\n", "seed"),
            http("200 OK", "Content-Type: text/plain\r\n", "welcome"),
        ]);
        let base = format!("http://127.0.0.1:{port}");
        fetch(&Url::parse(&format!("{base}/start")).unwrap()).unwrap();
        let f = fetch_post(&Url::parse(&format!("{base}/login")).unwrap(), "user=a&pw=p+w%26x").unwrap();
        h.join().unwrap();
        assert_eq!(f.body, b"welcome");
        let req = &heads.lock().unwrap()[1];
        assert!(req.starts_with("post /login http/1.1"), "{req}");
        assert!(req.contains("content-type: application/x-www-form-urlencoded"), "{req}");
        assert!(req.contains("content-length: 17"), "{req}");
        assert!(req.contains("post_ck=1"), "cookies go with the POST: {req}");
        assert!(req.ends_with("\r\n\r\nuser=a&pw=p+w%26x"), "body: {req}");
    }

    #[test]
    fn a_redirect_after_a_post_is_followed_with_a_get() {
        let (port, heads, h) = serve_recording(vec![
            http("303 See Other", "Location: /done\r\n", ""),
            http("200 OK", "Content-Type: text/plain\r\n", "thanks"),
        ]);
        let f = fetch_post(&Url::parse(&format!("http://127.0.0.1:{port}/submit")).unwrap(), "a=1").unwrap();
        h.join().unwrap();
        assert_eq!((f.url.path(), f.body.as_slice()), ("/done", &b"thanks"[..]));
        let heads = heads.lock().unwrap();
        assert!(heads[0].starts_with("post /submit"), "{}", heads[0]);
        assert!(heads[1].starts_with("get /done"), "{}", heads[1]);
        assert!(!heads[1].contains("a=1"), "the body is not re-sent");
    }

    #[test]
    fn posting_to_a_non_http_url_or_with_an_oversized_body_is_refused() {
        assert!(fetch_post(&Url::parse("file:///tmp/x.html").unwrap(), "a=1").is_err());
        assert!(fetch_post(&Url::parse("data:text/plain,hi").unwrap(), "a=1").is_err());
        let big = "x".repeat(MAX_POST_BODY + 1);
        assert!(fetch_post(&Url::parse("http://127.0.0.1:1/").unwrap(), &big).is_err());
    }

    // ---------- cookies ----------
    //
    // The cookie jar is process-global, so tests running in parallel share it:
    // each test uses its own cookie names, and assertions look for that
    // test's `name=value` pair rather than the whole `Cookie:` header.

    #[test]
    fn cookies_set_by_a_response_are_sent_back_on_later_requests() {
        let (port, heads, h) = serve_recording(vec![
            http("200 OK", "Set-Cookie: rt_sid=abc123; Path=/\r\nContent-Type: text/plain\r\n", "logged in"),
            http("200 OK", "Content-Type: text/plain\r\n", "next"),
        ]);
        let base = format!("http://127.0.0.1:{port}");
        fetch(&Url::parse(&format!("{base}/login")).unwrap()).unwrap();
        fetch(&Url::parse(&format!("{base}/account")).unwrap()).unwrap();
        h.join().unwrap();
        let heads = heads.lock().unwrap();
        assert!(!heads[0].contains("rt_sid"), "no cookie before the server set one");
        assert!(heads[1].contains("rt_sid=abc123"), "the cookie came back: {}", heads[1]);
    }

    #[test]
    fn cookies_are_not_sent_to_other_hosts_or_other_paths() {
        // set on 127.0.0.1 under /admin only
        let (port, _heads, h) = serve_recording(vec![http("200 OK", "Set-Cookie: iso_ck=secret; Path=/admin\r\n", "set")]);
        fetch(&Url::parse(&format!("http://127.0.0.1:{port}/admin/login")).unwrap()).unwrap();
        h.join().unwrap();
        // a different path on the same host
        let (p2, heads2, h2) = serve_recording(vec![http("200 OK", "", "x"), http("200 OK", "", "x")]);
        // (a second server on 127.0.0.1 shares the host, cookies aren't port-scoped)
        fetch(&Url::parse(&format!("http://127.0.0.1:{p2}/admin/panel")).unwrap()).unwrap();
        fetch(&Url::parse(&format!("http://127.0.0.1:{p2}/public")).unwrap()).unwrap();
        h2.join().unwrap();
        let heads2 = heads2.lock().unwrap();
        assert!(heads2[0].contains("iso_ck=secret"), "path /admin matches: {}", heads2[0]);
        assert!(!heads2[1].contains("iso_ck"), "path /public must not get it: {}", heads2[1]);
        // a different host name entirely (localhost is not 127.0.0.1 to a cookie jar)
        let (p3, heads3, h3) = serve_recording(vec![http("200 OK", "", "x")]);
        fetch(&Url::parse(&format!("http://localhost:{p3}/admin/panel")).unwrap()).unwrap();
        h3.join().unwrap();
        assert!(!heads3.lock().unwrap()[0].contains("iso_ck"), "cookies are per host");
    }

    #[test]
    fn expired_and_deleted_cookies_are_not_sent() {
        let (port, heads, h) = serve_recording(vec![
            http("200 OK", "Set-Cookie: exp_ck=1; Path=/\r\n", "set"),
            http("200 OK", "", "check"),
            http("200 OK", "Set-Cookie: exp_ck=; Path=/; Max-Age=0\r\n", "delete"),
            http("200 OK", "", "check again"),
        ]);
        let get = |p: &str| fetch(&Url::parse(&format!("http://127.0.0.1:{port}{p}")).unwrap()).unwrap();
        get("/a");
        get("/b");
        get("/c");
        get("/d");
        h.join().unwrap();
        let heads = heads.lock().unwrap();
        assert!(heads[1].contains("exp_ck=1"), "sent while valid");
        assert!(!heads[3].contains("exp_ck"), "Max-Age=0 removes it: {}", heads[3]);
    }

    #[test]
    fn the_cookie_jar_is_shared_between_threads_and_survives_redirects() {
        // login sets a cookie then redirects; the redirect target (and a request from
        // another thread, as the loader's workers make) must carry it
        let (port, heads, h) = serve_recording(vec![
            http("302 Found", "Set-Cookie: thr_ck=T; Path=/\r\nLocation: /home\r\n", ""),
            http("200 OK", "Content-Type: text/plain\r\n", "home"),
            http("200 OK", "Content-Type: text/plain\r\n", "other thread"),
        ]);
        let base = format!("http://127.0.0.1:{port}");
        let f = fetch(&Url::parse(&format!("{base}/login")).unwrap()).unwrap();
        assert_eq!(f.body, b"home");
        let url = Url::parse(&format!("{base}/data")).unwrap();
        std::thread::spawn(move || fetch(&url).unwrap()).join().unwrap();
        h.join().unwrap();
        let heads = heads.lock().unwrap();
        // (request heads are lower-cased by the recorder, hence `thr_ck=t`)
        assert!(heads[1].contains("thr_ck=t"), "redirect target carries the cookie: {}", heads[1]);
        assert!(heads[2].contains("thr_ck=t"), "other threads share the jar: {}", heads[2]);
    }

    #[test]
    fn cross_site_subresources_neither_set_nor_receive_cookies() {
        // The page is on 127.0.0.1; the "tracker" is localhost, a different site.
        let page = Url::parse("http://127.0.0.1:1/index.html").unwrap();
        let (port, heads, h) = serve_recording(vec![
            http("200 OK", "Set-Cookie: tp_ck=track; Path=/\r\n", "pixel"),
            http("200 OK", "", "later visit"),
        ]);
        let tracker = |p: &str| Url::parse(&format!("http://localhost:{port}{p}")).unwrap();
        fetch_subresource(&tracker("/pixel.png"), &page).unwrap();
        // visiting the tracker's own site afterwards must not find a cookie
        fetch(&tracker("/home")).unwrap();
        h.join().unwrap();
        assert!(!heads.lock().unwrap()[1].contains("tp_ck"), "third-party Set-Cookie was stored");

        // and a cookie the user already has for that site isn't sent along with the pixel
        let (port2, heads2, h2) = serve_recording(vec![
            http("200 OK", "Set-Cookie: fp_ck=mine; Path=/\r\n", "visit"),
            http("200 OK", "", "pixel"),
            http("200 OK", "", "same-site resource"),
        ]);
        let own = |p: &str| Url::parse(&format!("http://localhost:{port2}{p}")).unwrap();
        fetch(&own("/visit")).unwrap();
        fetch_subresource(&own("/pixel.png"), &page).unwrap();
        fetch_subresource(&own("/style.css"), &own("/visit")).unwrap();
        h2.join().unwrap();
        let heads2 = heads2.lock().unwrap();
        assert!(!heads2[1].contains("fp_ck"), "cookie leaked to a cross-site request: {}", heads2[1]);
        assert!(heads2[2].contains("fp_ck=mine"), "same-site subresources keep their cookies: {}", heads2[2]);
    }

    #[test]
    fn file_urls_read_files_and_guess_types() {
        let dir = std::env::temp_dir().join(format!("mybrowser-net-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.html"), "<p>x</p>").unwrap();
        std::fs::write(dir.join("noext"), "<html>sniffed</html>").unwrap();
        let f = fetch(&Url::from_file_path(dir.join("a.html")).unwrap()).unwrap();
        assert_eq!((f.content_type.as_str(), f.body.as_slice()), ("text/html", &b"<p>x</p>"[..]));
        assert_eq!(fetch(&Url::from_file_path(dir.join("noext")).unwrap()).unwrap().content_type, "text/html");
        assert!(fetch(&Url::from_file_path(&dir).unwrap()).is_err(), "directories are not documents");
    }

    #[test]
    fn special_files_do_not_hang_or_exhaust_memory() {
        // /dev/zero reports length 0 but never ends: it used to be read until memory ran out
        for dev in ["/dev/zero", "/dev/urandom", "/dev/null"] {
            if std::path::Path::new(dev).exists() {
                let t = std::time::Instant::now();
                assert!(fetch(&Url::from_file_path(dev).unwrap()).is_err(), "{dev}");
                assert!(t.elapsed().as_secs() < 2);
            }
        }
    }

    #[test]
    fn data_urls() {
        let f = fetch(&Url::parse("data:text/plain;base64,SGVsbG8sIHdvcmxkIQ==").unwrap()).unwrap();
        assert_eq!((f.content_type.as_str(), f.body.as_slice()), ("text/plain", &b"Hello, world!"[..]));
        let f = fetch(&Url::parse("data:,a%20b%3Cc").unwrap()).unwrap();
        assert_eq!(f.body, b"a b<c");
        let f = fetch(&Url::parse("data:text/html;charset=utf-8,<p>hi</p>").unwrap()).unwrap();
        assert_eq!((f.content_type.as_str(), f.charset.as_deref()), ("text/html", Some("utf-8")));
        assert!(fetch(&Url::parse("data:text/plain;base64,@@@@").unwrap()).is_err());
        assert!(fetch(&Url::parse("data:nocomma").unwrap()).is_err());
    }

    #[test]
    fn data_url_fragment_and_base64_case_and_garbage() {
        // the fragment isn't data
        assert_eq!(fetch(&Url::parse("data:text/plain,abc#frag").unwrap()).unwrap().body, b"abc");
        // `;BASE64` is case-insensitive
        assert_eq!(fetch(&Url::parse("data:text/plain;BASE64,aGk=").unwrap()).unwrap().body, b"hi");
        // junk after padding is an error, not silently ignored
        assert!(fetch(&Url::parse("data:text/plain;base64,aGk=garbage").unwrap()).is_err());
    }

    #[test]
    fn base64_variants() {
        assert_eq!(base64_decode(b"aGk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"aGk"), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"a G\nk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"_-8"), Some(vec![0xFF, 0xEF]));
        assert_eq!(base64_decode(b"aGk==="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"!"), None);
        assert_eq!(base64_decode(b"aGk=x"), None);
    }

    #[test]
    fn text_decoding_handles_bom_header_and_meta_charsets() {
        let html = |b: &[u8], cs: Option<&str>| decode_text(b, cs, true);
        assert_eq!(html(b"\xEF\xBB\xBFhi \xC3\xA9", None), "hi é");
        assert_eq!(html(b"caf\xE9", Some("iso-8859-1")), "café");
        assert_eq!(html(b"\x93quoted\x94 \x80", Some("windows-1252")), "“quoted” €");
        assert_eq!(html(b"<meta charset=\"ISO-8859-1\">caf\xE9", None), "<meta charset=\"ISO-8859-1\">café");
        assert_eq!(html(b"<meta http-equiv=x content='text/html; charset=windows-1252'>\x92", None).chars().last(), Some('’'));
        // invalid UTF-8 never panics
        assert!(html(b"\xFF\xFD\xFD", None).contains('\u{fffd}'));
        // a BOM beats a conflicting declaration
        assert_eq!(html(b"\xEF\xBB\xBF\xC3\xA9", Some("iso-8859-1")), "é");
    }

    #[test]
    fn meta_charset_only_counts_in_real_meta_tags_of_html() {
        let utf8 = "Café —".as_bytes();
        // plain text and other non-HTML types never sniff a meta charset
        let mut txt = b"<meta charset=iso-8859-1>\n".to_vec();
        txt.extend_from_slice(utf8);
        assert!(decode_text(&txt, None, false).ends_with("Café —"));
        // `charset=` in a script, a comment, or the body text is not a declaration
        for page in [
            "<head><script>var s='charset=windows-1252'</script></head>Café —",
            "<!-- charset=windows-1252 -->Café —",
            "<p>use charset=iso-8859-1 for old pages</p>Café —",
            "<head></head><meta charset=windows-1252>Café —",
        ] {
            assert!(decode_text(page.as_bytes(), None, true).ends_with("Café —"), "{page}");
        }
    }

    #[test]
    fn utf16_text_is_decoded() {
        let le: Vec<u8> = [0xFF, 0xFE].into_iter().chain("héllo".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&le, None, false), "héllo");
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("日本".encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
        assert_eq!(decode_text(&be, None, false), "日本");
        assert_eq!(decode_text(&[0xFF, 0xFE, 0x41], None, false), ""); // truncated unit is dropped, no panic
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(sniff(b"  \n<!doctype html>"), "text/html");
        assert_eq!(sniff(b"<p>hi"), "text/html");
        assert_eq!(sniff(b"just words"), "text/plain");
        assert_eq!(sniff(b"<3 love letters"), "text/plain");
        assert_eq!(sniff(&[0, 159, 146, 150]), "application/octet-stream");
        assert_eq!(sniff(&[0xFF, 0xFE, b'h', 0]), "text/plain");
        // a big binary blob without NULs in its first 4 KB is not text
        let blob: Vec<u8> = (0..8000u32).map(|i| 0x80 | (i % 0x40) as u8).collect();
        assert_eq!(sniff(&blob), "application/octet-stream");
        // a multi-byte char split by the 4 KB boundary is still text
        let mut text = "a".repeat(4095).into_bytes();
        text.extend_from_slice("é".as_bytes());
        assert_eq!(sniff(&text), "text/plain");
    }
}
