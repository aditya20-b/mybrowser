//! The browser's cookie jar and the rules for what may go into it.
//!
//! One jar lives for the life of the process (nothing is written to disk). On top
//! of the standard storage rules (`cookie_store`: host/path matching, expiry,
//! `Max-Age`) this applies the protections a browser needs:
//!
//! * a cookie's `Domain` may not be a public suffix (`co.uk`, `github.io`): no supercookies
//! * `Secure` cookies are only accepted from https (or loopback) and never sent over plain http
//! * `__Secure-` and `__Host-` prefixes are enforced
//! * size and count limits, so one site can't make every later request to a host fail
//! * cross-site subresources (a tracking pixel on another domain) neither send nor set cookies
use cookie_store::{Cookie, CookieDomain, CookieStore};
use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, OnceLock};
use url::{Host, Url};

/// Longest `name=value` we keep (browsers use about 4 KB).
const MAX_COOKIE_BYTES: usize = 4096;
/// Most cookies kept per domain.
const MAX_PER_DOMAIN: usize = 50;
/// Most cookies kept in total.
const MAX_TOTAL: usize = 3000;
/// Longest `Cookie:` request header we send. The HTTP client's buffer is finite, so an
/// oversized header would make every request to the host fail.
const MAX_HEADER: usize = 8000;

pub struct Jar {
    store: CookieStore,
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// The "site" of a URL: its registrable domain (`a.example.co.uk` -> `example.co.uk`),
/// or the host itself for IP addresses and names the suffix list doesn't know.
pub fn site_of(url: &Url) -> String {
    match url.host() {
        Some(Host::Domain(d)) => {
            let d = d.to_ascii_lowercase();
            psl::domain_str(&d).map(String::from).unwrap_or(d)
        }
        Some(h) => h.to_string(),
        None => String::new(),
    }
}

/// Are both URLs on the same site?
pub fn same_site(a: &Url, b: &Url) -> bool {
    let site = site_of(a);
    !site.is_empty() && site == site_of(b)
}

/// Is this a name or value we are willing to send back in a `Cookie:` header?
fn sendable(s: &str, allow_empty: bool) -> bool {
    (allow_empty || !s.is_empty())
        && s.bytes().all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b';' | b',' | b'\\' | b'"' | b'=' | b' '))
}

impl Jar {
    pub fn new() -> Jar {
        Jar { store: CookieStore::new() }
    }

    /// Process one `Set-Cookie` header received from `url`. Returns whether the cookie was kept.
    pub fn accept(&mut self, url: &Url, header: &str) -> bool {
        if header.len() > MAX_COOKIE_BYTES + 1024 {
            return false; // (the extra allows for attributes)
        }
        let Ok(cookie) = Cookie::parse(header.to_string(), url) else { return false };
        if cookie.name().len() + cookie.value().len() > MAX_COOKIE_BYTES {
            return false;
        }
        let https = url.scheme() == "https";
        let secure = cookie.secure() == Some(true);
        if secure && !https && !is_loopback(url) {
            return false; // a network attacker could have forged it
        }
        let lower = cookie.name().to_ascii_lowercase();
        if lower.starts_with("__secure-") && !(secure && https) {
            return false;
        }
        if lower.starts_with("__host-") {
            let at_root = cookie.path.is_from_path_attr() && String::from(&cookie.path) == "/";
            if !(secure && https && at_root && matches!(cookie.domain, CookieDomain::HostOnly(_))) {
                return false;
            }
        }
        // `Domain=co.uk` would put one cookie in front of every .co.uk site
        if let CookieDomain::Suffix(d) = &cookie.domain {
            if psl::suffix_str(d) == Some(d.as_str()) {
                return false;
            }
        }
        if self.store.insert(cookie.into_owned(), url).is_err() {
            return false;
        }
        self.prune();
        true
    }

    /// The value of the `Cookie:` header for a request to `url`, if any cookies apply.
    pub fn header_for(&self, url: &Url) -> Option<String> {
        let mut header = String::new();
        for (name, value) in self.store.get_request_values(url) {
            if !sendable(name, false) || !sendable(value, true) {
                continue;
            }
            let add = name.len() + value.len() + 3;
            if header.len() + add > MAX_HEADER {
                break;
            }
            if !header.is_empty() {
                header.push_str("; ");
            }
            header.push_str(name);
            header.push('=');
            header.push_str(value);
        }
        if header.is_empty() { None } else { Some(header) }
    }

    /// How many cookies are stored (including expired ones not yet purged).
    pub fn len(&self) -> usize {
        self.store.iter_any().count()
    }

    /// Drop cookies beyond the per-domain and total limits.
    fn prune(&mut self) {
        // (domain, path, name) for every stored cookie
        let all: Vec<(String, String, String)> = self
            .store
            .iter_any()
            .map(|c| (c.domain.as_cow().map(|d| d.into_owned()).unwrap_or_default(), String::from(&c.path), c.name().to_string()))
            .collect();
        let mut per_domain: HashMap<&str, usize> = HashMap::new();
        for (domain, _, _) in &all {
            *per_domain.entry(domain.as_str()).or_default() += 1;
        }
        let mut excess: HashMap<&str, usize> = per_domain.into_iter().map(|(d, n)| (d, n.saturating_sub(MAX_PER_DOMAIN))).collect();
        let mut over_total = all.len().saturating_sub(MAX_TOTAL);
        let mut doomed = vec![];
        for entry in &all {
            let e = excess.get_mut(entry.0.as_str()).expect("counted above");
            if *e > 0 {
                *e -= 1;
                doomed.push(entry);
            } else if over_total > 0 {
                over_total -= 1;
                doomed.push(entry);
            }
        }
        for (domain, path, name) in doomed {
            self.store.remove(domain, path, name);
        }
    }
}

impl Default for Jar {
    fn default() -> Jar {
        Jar::new()
    }
}

/// The process-wide jar shared by every request and every loader thread.
pub fn global() -> MutexGuard<'static, Jar> {
    static JAR: OnceLock<Mutex<Jar>> = OnceLock::new();
    JAR.get_or_init(|| Mutex::new(Jar::new())).lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn header(jar: &Jar, url: &str) -> String {
        jar.header_for(&u(url)).unwrap_or_default()
    }

    #[test]
    fn ordinary_cookies_round_trip_with_host_and_path_scoping() {
        let mut jar = Jar::new();
        assert!(jar.accept(&u("https://www.example.com/a/b"), "sid=1; Path=/a"));
        assert!(jar.accept(&u("https://www.example.com/"), "all=2; Domain=example.com"));
        assert_eq!(header(&jar, "https://www.example.com/a/x"), "sid=1; all=2");
        assert_eq!(header(&jar, "https://www.example.com/other"), "all=2");
        assert_eq!(header(&jar, "https://api.example.com/"), "all=2", "Domain cookies reach subdomains");
        assert_eq!(header(&jar, "https://example.org/"), "");
    }

    #[test]
    fn a_public_suffix_domain_is_refused() {
        // Domain=co.uk used to put one site's cookie in front of every .co.uk site
        let mut jar = Jar::new();
        for (page, set) in [
            ("https://evil.co.uk/", "track=1; Domain=co.uk"),
            ("https://evil.co.uk/", "track=1; Domain=.co.uk"),
            ("https://evil.com/", "track=1; Domain=com"),
            ("https://me.github.io/", "track=1; Domain=github.io"),
            ("https://a.example.test/", "track=1; Domain=test"),
        ] {
            assert!(!jar.accept(&u(page), set), "{set} from {page}");
        }
        assert_eq!(jar.len(), 0);
        // a real registrable domain is still fine, even under a multi-part suffix
        assert!(jar.accept(&u("https://shop.example.co.uk/"), "ok=1; Domain=example.co.uk"));
        assert_eq!(header(&jar, "https://other.example.co.uk/"), "ok=1");
        assert_eq!(header(&jar, "https://victim.co.uk/"), "");
    }

    #[test]
    fn secure_cookies_need_https_and_are_never_sent_over_http() {
        let mut jar = Jar::new();
        assert!(!jar.accept(&u("http://example.com/"), "s=1; Secure"), "can't be set over plain http");
        assert!(jar.accept(&u("https://example.com/"), "s=1; Secure"));
        assert_eq!(header(&jar, "https://example.com/"), "s=1");
        assert_eq!(header(&jar, "http://example.com/"), "", "not sent in the clear");
        // loopback development servers may use Secure over http
        assert!(jar.accept(&u("http://localhost:3000/"), "dev=1; Secure"));
    }

    #[test]
    fn cookie_name_prefixes_are_enforced() {
        let mut jar = Jar::new();
        let https = u("https://example.com/");
        assert!(!jar.accept(&u("http://example.com/"), "__Secure-a=1; Secure"));
        assert!(!jar.accept(&https, "__Secure-a=1"), "needs Secure");
        assert!(jar.accept(&https, "__Secure-a=1; Secure"));
        assert!(!jar.accept(&https, "__Host-b=1; Secure; Path=/; Domain=example.com"), "no Domain allowed");
        assert!(!jar.accept(&https, "__Host-b=1; Secure; Path=/x"), "Path must be /");
        assert!(!jar.accept(&https, "__Host-b=1; Secure"), "the Path=/ attribute must be explicit");
        assert!(!jar.accept(&https, "__Host-b=1; Path=/"), "needs Secure");
        assert!(jar.accept(&https, "__Host-c=1; Secure; Path=/"));
        assert!(!jar.accept(&https, "__HOST-d=1; Path=/"), "prefix matching ignores case");
    }

    #[test]
    fn oversized_and_excess_cookies_are_limited_and_the_header_stays_small() {
        let mut jar = Jar::new();
        let url = u("https://spam.example.com/");
        assert!(!jar.accept(&url, &format!("big={}", "x".repeat(5000))), "a 5 KB cookie is refused");
        for i in 0..200 {
            jar.accept(&url, &format!("c{i}={}; Path=/", "v".repeat(3000)));
        }
        assert!(jar.len() <= MAX_PER_DOMAIN, "{} cookies kept", jar.len());
        let h = header(&jar, "https://spam.example.com/");
        assert!(!h.is_empty() && h.len() <= MAX_HEADER, "header is {} bytes", h.len());
        // other domains are untouched and still work
        assert!(jar.accept(&u("https://other.example.org/"), "fine=1"));
        assert_eq!(header(&jar, "https://other.example.org/"), "fine=1");
    }

    #[test]
    fn cookies_that_cannot_be_sent_back_safely_are_skipped() {
        let mut jar = Jar::new();
        let url = u("https://example.com/");
        jar.accept(&url, "good=1");
        jar.accept(&url, "sp=a b");
        jar.accept(&url, "uni=caf\u{e9}");
        assert_eq!(header(&jar, "https://example.com/"), "good=1");
    }

    #[test]
    fn deleting_a_cookie_with_max_age_zero_works() {
        let mut jar = Jar::new();
        let url = u("https://example.com/");
        jar.accept(&url, "gone=1; Path=/");
        assert_eq!(header(&jar, "https://example.com/"), "gone=1");
        jar.accept(&url, "gone=; Path=/; Max-Age=0");
        assert_eq!(header(&jar, "https://example.com/"), "");
    }

    #[test]
    fn sites_are_registrable_domains() {
        assert!(same_site(&u("https://www.example.com/"), &u("https://cdn.example.com/x")));
        assert!(same_site(&u("https://a.example.co.uk/"), &u("https://b.example.co.uk/")));
        assert!(!same_site(&u("https://a.co.uk/"), &u("https://b.co.uk/")));
        assert!(!same_site(&u("https://me.github.io/"), &u("https://you.github.io/")));
        assert!(!same_site(&u("https://example.com/"), &u("https://example.org/")));
        assert!(same_site(&u("http://127.0.0.1:1/"), &u("http://127.0.0.1:2/")));
        assert!(!same_site(&u("http://127.0.0.1/"), &u("http://localhost/")));
        assert!(!same_site(&u("file:///a.html"), &u("file:///b.html")), "no host, no site");
    }
}
