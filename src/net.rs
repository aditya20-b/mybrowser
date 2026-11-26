//! Resource loading for `file:`, `http:`, `https:` and `data:` URLs.
use std::sync::OnceLock;
use std::time::Duration;
use url::Url;

/// Largest response body we will read (pages, stylesheets, images).
pub const MAX_BODY: u64 = 32 * 1024 * 1024;

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

pub fn fetch(url: &Url) -> Result<Fetched, String> {
    match url.scheme() {
        "file" => fetch_file(url),
        "http" | "https" => fetch_http(url),
        "data" => fetch_data(url),
        other => Err(format!("unsupported URL scheme '{other}:'")),
    }
}

fn fetch_file(url: &Url) -> Result<Fetched, String> {
    let path = url.to_file_path().map_err(|_| format!("not a local file path: {url}"))?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory", path.display()));
    }
    if meta.len() > MAX_BODY {
        return Err(format!("{} is too large ({} bytes)", path.display(), meta.len()));
    }
    let body = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
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
            .max_redirects(10)
            .user_agent("Mozilla/5.0 (compatible; mybrowser/0.1)")
            // An HTTP error status still has a body worth showing (404 pages etc.).
            .http_status_as_error(false)
            .build()
            .into()
    })
}

fn fetch_http(url: &Url) -> Result<Fetched, String> {
    use ureq::ResponseExt;
    let mut resp = agent()
        .get(url.as_str())
        .header("Accept", ACCEPT)
        .header("Accept-Language", "en")
        .call()
        .map_err(|e| format!("{url}: {e}"))?;
    let final_url = Url::parse(&resp.get_uri().to_string()).unwrap_or_else(|_| url.clone());
    let status = resp.status().as_u16();
    let content_type = resp.body().mime_type().unwrap_or("").to_ascii_lowercase();
    let charset = resp.body().charset().map(|c| c.to_ascii_lowercase());
    let body = resp
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|e| format!("reading {url}: {e}"))?;
    let content_type = if content_type.is_empty() { sniff(&body).to_string() } else { content_type };
    Ok(Fetched { url: final_url, status, content_type, charset, body })
}

fn fetch_data(url: &Url) -> Result<Fetched, String> {
    let rest = url.as_str().strip_prefix("data:").ok_or("not a data: URL")?;
    let (meta, data) = rest.split_once(',').ok_or("malformed data: URL (no comma)")?;
    let (meta, is_base64) = match meta.strip_suffix(";base64") {
        Some(m) => (m, true),
        None => (meta, false),
    };
    let mut params = meta.split(';');
    let mime = params.next().filter(|m| !m.is_empty()).unwrap_or("text/plain").to_ascii_lowercase();
    let charset = params.find_map(|p| p.trim().strip_prefix("charset=")).map(|c| c.to_ascii_lowercase());
    let raw = percent_decode_bytes(data);
    let body = if is_base64 { base64_decode(&raw).ok_or("invalid base64 in data: URL")? } else { raw };
    if body.len() as u64 > MAX_BODY {
        return Err("data: URL too large".into());
    }
    Ok(Fetched { url: url.clone(), status: 200, content_type: mime, charset, body })
}

/// Guess a content type from the bytes themselves.
pub fn sniff(body: &[u8]) -> &'static str {
    if body.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if body.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if body.starts_with(b"GIF8") {
        "image/gif"
    } else if body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<') {
        "text/html"
    } else if std::str::from_utf8(&body[..body.len().min(4096)]).is_ok() || body.len() > 4096 {
        "text/plain"
    } else {
        "application/octet-stream"
    }
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

/// Standard and URL-safe base64; whitespace and padding are ignored.
pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in input {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            c if c.is_ascii_whitespace() => continue,
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

/// Turn response bytes into text, honouring a UTF-8 BOM, the HTTP charset, or a
/// `<meta charset>` near the top. UTF-8 is the default; Latin-1 and
/// Windows-1252 are decoded properly, and anything else falls back to lossy UTF-8.
pub fn decode_text(body: &[u8], header_charset: Option<&str>) -> String {
    let (body, bom) = match body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        Some(rest) => (rest, true),
        None => (body, false),
    };
    let charset = if bom { None } else { header_charset.map(String::from).or_else(|| meta_charset(body)) };
    match charset.as_deref() {
        Some("iso-8859-1" | "latin1" | "windows-1252" | "cp1252" | "us-ascii" | "ascii") => {
            body.iter().map(|&b| windows_1252(b)).collect()
        }
        _ => String::from_utf8_lossy(body).into_owned(),
    }
}

fn meta_charset(body: &[u8]) -> Option<String> {
    let head = String::from_utf8_lossy(&body[..body.len().min(2048)]).to_ascii_lowercase();
    let at = head.find("charset=")? + "charset=".len();
    let value: String = head[at..]
        .trim_start_matches(['"', '\''])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!value.is_empty()).then_some(value)
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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve one canned response per connection on a free local port.
    fn serve(responses: Vec<String>) -> (u16, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for resp in responses {
                let (mut sock, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let mut seen = vec![];
                while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = sock.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&buf[..n]);
                }
                sock.write_all(resp.as_bytes()).unwrap();
            }
        });
        (port, handle)
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
    fn http_error_statuses_still_deliver_their_body() {
        let (port, h) = serve(vec![http("404 Not Found", "Content-Type: text/html\r\n", "<h1>nope</h1>")]);
        let f = fetch(&Url::parse(&format!("http://127.0.0.1:{port}/missing")).unwrap()).unwrap();
        h.join().unwrap();
        assert_eq!(f.status, 404);
        assert_eq!(f.body, b"<h1>nope</h1>");
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
    fn base64_variants() {
        assert_eq!(base64_decode(b"aGk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"aGk"), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"a G\nk="), Some(b"hi".to_vec()));
        assert_eq!(base64_decode(b"_-8"), Some(vec![0xFF, 0xEF]));
        assert_eq!(base64_decode(b"!"), None);
    }

    #[test]
    fn text_decoding_handles_bom_header_and_meta_charsets() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhi \xC3\xA9", None), "hi é");
        assert_eq!(decode_text(b"caf\xE9", Some("iso-8859-1")), "café");
        assert_eq!(decode_text(b"\x93quoted\x94 \x80", Some("windows-1252")), "“quoted” €");
        assert_eq!(decode_text(b"<meta charset=\"ISO-8859-1\">caf\xE9", None), "<meta charset=\"ISO-8859-1\">café");
        assert_eq!(decode_text(b"<meta http-equiv=x content='text/html; charset=windows-1252'>\x92", None).chars().last(), Some('’'));
        // invalid UTF-8 never panics
        assert!(decode_text(b"\xFF\xFE\xFD", None).contains('\u{fffd}'));
        // a BOM beats a conflicting declaration
        assert_eq!(decode_text(b"\xEF\xBB\xBF\xC3\xA9", Some("iso-8859-1")), "é");
    }

    #[test]
    fn sniffing() {
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(sniff(b"  \n<!doctype html>"), "text/html");
        assert_eq!(sniff(b"just words"), "text/plain");
        assert_eq!(sniff(&[0, 159, 146, 150]), "application/octet-stream");
    }
}
