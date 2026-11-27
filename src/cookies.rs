//! The browser's cookie jar and the rules for what may go into it.
//!
//! One jar lives for the life of the process (cookies with an expiry are also saved to disk and read back at start). On top
//! of the standard storage rules (`cookie_store`: host/path matching, expiry,
//! `Max-Age`) this applies the protections a browser needs:
//!
//! * a cookie's `Domain` may not be a public suffix (`co.uk`, `github.io`): no supercookies
//! * `Secure` cookies are only accepted from https (or loopback), never sent over
//!   plain http, and can't be overwritten by a plain-http response
//! * `__Secure-` and `__Host-` prefixes are enforced
//! * `SameSite`: cookies without the attribute behave as `Lax`, so a request that
//!   another site starts (a link, a form POST) doesn't carry them unless it is a
//!   plain top-level GET; `Strict` cookies never go cross-site; `None` needs `Secure`
//! * size and count limits, so one site can't make every later request to a host fail;
//!   when a site goes over, *its own* oldest cookies are evicted, never another site's
//! * cross-site subresources (a tracking pixel on another domain) neither send nor set cookies
use cookie::SameSite;
use cookie_store::{Cookie, CookieDomain, CookieStore};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, MutexGuard, OnceLock};
use url::{Host, Url};

/// Longest `name=value` we keep (browsers use about 4 KB).
const MAX_COOKIE_BYTES: usize = 4096;
/// Most cookies kept per host (or `Domain=` value).
const MAX_PER_DOMAIN: usize = 50;
/// Most cookies kept per site (registrable domain, all its subdomains together).
const MAX_PER_SITE: usize = 180;
/// Most cookies kept in total.
const MAX_TOTAL: usize = 3000;
/// Longest `Cookie:` request header we send. The HTTP client's buffer is finite, so an
/// oversized header would make every request to the host fail.
const MAX_HEADER: usize = 8000;

/// (domain, path, name): how the store identifies a cookie.
type Key = (String, String, String);

/// Who started a request, which decides which cookies may go with it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ctx {
    /// The user typed the address, or the request is from the same site.
    SameSite,
    /// Another site started a top-level GET (the user followed a link from elsewhere).
    CrossSiteGet,
    /// Another site started a POST (a form on a different site).
    CrossSitePost,
}

pub struct Jar {
    /// The cookie file exists but could not be read: it is left alone (never overwritten).
    no_save: bool,
    /// Cookies are being read back from the file: the limits are applied once, at the end.
    loading: bool,
    /// A persistent cookie was set or deleted since the jar was last written to disk.
    dirty: bool,
    store: CookieStore,
    /// When each cookie was last set, so the oldest of an over-limit site go first.
    seq: HashMap<Key, u64>,
    counter: u64,
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
        Some(Host::Domain(d)) => site_of_domain(d),
        Some(h) => h.to_string(),
        None => String::new(),
    }
}

/// The registrable domain of a cookie's domain string (IP addresses stay as they are).
fn site_of_domain(d: &str) -> String {
    let d = d.trim_start_matches('.').to_ascii_lowercase();
    if d.parse::<std::net::IpAddr>().is_ok() {
        return d;
    }
    psl::domain_str(&d).map(String::from).unwrap_or(d)
}

/// Are both URLs on the same site?
pub fn same_site(a: &Url, b: &Url) -> bool {
    let site = site_of(a);
    !site.is_empty() && site == site_of(b)
}

/// How a request from `initiator` (the page the user was on) to `target` relates
/// to cookies. No initiator means the user typed the address or used history.
pub fn context(initiator: Option<&Url>, target: &Url, is_post: bool) -> Ctx {
    match initiator {
        Some(from) if !same_site(from, target) => {
            if is_post { Ctx::CrossSitePost } else { Ctx::CrossSiteGet }
        }
        _ => Ctx::SameSite,
    }
}

/// May a cookie with this `SameSite` attribute go with a request in `ctx`?
fn same_site_allows(attr: Option<SameSite>, ctx: Ctx) -> bool {
    match (ctx, attr) {
        (Ctx::SameSite, _) => true,
        (Ctx::CrossSiteGet, Some(SameSite::Strict)) => false,
        (Ctx::CrossSiteGet, _) => true, // Lax, which is also the default
        (Ctx::CrossSitePost, Some(SameSite::None)) => true,
        (Ctx::CrossSitePost, _) => false,
    }
}

/// Is this a name or value we are willing to send back in a `Cookie:` header?
fn sendable(s: &str, allow_empty: bool) -> bool {
    (allow_empty || !s.is_empty())
        && s.bytes().all(|b| (0x21..=0x7e).contains(&b) && !matches!(b, b';' | b',' | b'\\' | b'"' | b'=' | b' '))
}

fn key_of(c: &Cookie<'_>) -> Key {
    (c.domain.as_cow().map(|d| d.into_owned()).unwrap_or_default(), String::from(&c.path), c.name().to_string())
}

impl Jar {
    pub fn new() -> Jar {
        Jar { no_save: false, loading: false, dirty: false, store: CookieStore::new(), seq: HashMap::new(), counter: 0 }
    }

    /// The cookies that outlive the session, one per line, as tab-separated fields:
    /// `scheme host-only domain path name value expires secure http-only same-site`.
    /// (Session cookies are never written down.)
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for c in self.store.iter_unexpired() {
            // (never more than can be read back, or the whole file would be refused next time)
            if out.len() as u64 > MAX_FILE_BYTES / 2 {
                break;
            }
            let cookie_store::CookieExpiration::AtUtc(at) = &c.expires else { continue };
            let Some(domain) = c.domain.as_cow().map(|d| d.into_owned()) else { continue };
            let (name, value) = c.name_value();
            let path = String::from(&c.path);
            let fields = [domain.as_str(), path.as_str(), name, value];
            if fields.iter().any(|f| f.chars().any(|ch| ch.is_control())) {
                continue;
            }
            let secure = c.secure() == Some(true);
            let same_site = match c.same_site() {
                Some(SameSite::Strict) => "Strict",
                Some(SameSite::None) => "None",
                Some(SameSite::Lax) => "Lax",
                None => "",
            };
            out.push_str(&format!(
                "{}\t{}\t{domain}\t{path}\t{name}\t{value}\t{}\t{}\t{}\t{same_site}\n",
                if secure { "https" } else { "http" },
                u8::from(matches!(c.domain, CookieDomain::HostOnly(_))),
                at.unix_timestamp(),
                u8::from(secure),
                u8::from(c.http_only() == Some(true)),
            ));
        }
        out
    }

    /// Take cookies back from `to_text`. Each goes through `accept`, so a hand-edited or damaged
    /// file can't smuggle in anything the rules would refuse.
    pub fn load_text(&mut self, text: &str) {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
        self.loading = true;
        for line in text.lines().take(MAX_TOTAL) {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 10 || !matches!(f[0], "http" | "https") {
                continue;
            }
            let (host_only, domain, path, name, value) = (f[1] == "1", f[2], f[3], f[4], f[5]);
            let Ok(expires) = f[6].parse::<i64>() else { continue };
            // (a colon is only for an IPv6 address, which is written in brackets)
            let colon_ok = domain.starts_with('[') && domain.ends_with(']');
            if expires <= now || domain.is_empty() || (domain.contains(':') && !colon_ok) || domain.contains(['/', '@', ' ', ';']) || !path.starts_with('/') {
                continue;
            }
            // (a value that holds `;` would add attributes to the cookie it is spliced into)
            if [name, value, path].iter().any(|s| s.contains(';') || s.chars().any(|c| c.is_control())) {
                continue;
            }
            let Ok(url) = Url::parse(&format!("{}://{}/", f[0], domain.trim_start_matches('.'))) else { continue };
            let mut header = format!("{name}={value}; Path={path}; Max-Age={}", expires - now);
            if !host_only {
                header.push_str(&format!("; Domain={domain}"));
            }
            if f[7] == "1" {
                header.push_str("; Secure");
            }
            if f[8] == "1" {
                header.push_str("; HttpOnly");
            }
            if matches!(f[9], "Lax" | "Strict" | "None") {
                header.push_str(&format!("; SameSite={}", f[9]));
            }
            self.accept(&url, &header);
        }
        self.loading = false;
        self.prune();
        self.dirty = false;
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
        if cookie.same_site() == Some(SameSite::None) && !secure {
            return false; // SameSite=None is only for Secure cookies
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
        let key = key_of(&cookie);
        // A plain-http response must not replace a Secure cookie (a network attacker could
        // otherwise plant a value in a session that https then trusts).
        if !https {
            if let Some(old) = self.store.get(&key.0, &key.1, &key.2) {
                if old.secure() == Some(true) {
                    return false;
                }
            }
        }
        let expired = cookie.is_expired();
        let persistent = cookie.is_persistent();
        let replaces_persistent = self.store.get(&key.0, &key.1, &key.2).is_some_and(|c| c.is_persistent());
        if self.store.insert(cookie.into_owned(), url).is_err() {
            return false;
        }
        // (a session cookie replacing a persistent one, or a deletion, changes the file as well)
        self.dirty |= persistent || expired || replaces_persistent;
        if expired {
            self.seq.remove(&key); // it was a deletion
        } else {
            self.counter += 1;
            self.seq.insert(key, self.counter);
        }
        if !self.loading {
            self.prune();
        }
        true
    }

    /// The value of the `Cookie:` header for a request to `url` started in `ctx`, if any
    /// cookies apply. Longer paths come first, then older cookies (RFC 6265).
    pub fn header_for(&self, url: &Url, ctx: Ctx) -> Option<String> {
        let mut matching = self.store.matches(url);
        matching.retain(|c| same_site_allows(c.same_site(), ctx));
        matching.sort_by_cached_key(|c| {
            let key = key_of(c);
            let age = self.seq.get(&key).copied().unwrap_or(0);
            (std::cmp::Reverse(key.1.len()), age)
        });
        let mut header = String::new();
        for c in matching {
            let (name, value) = c.name_value();
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

    /// Enforce the limits. A host, then a whole site, that is over its share loses its
    /// own oldest cookies; only if the jar as a whole is still over does the globally
    /// oldest cookie go.
    fn prune(&mut self) {
        let all: Vec<(Key, String, u64)> = self
            .store
            .iter_any()
            .map(|c| {
                let key = key_of(c);
                let age = self.seq.get(&key).copied().unwrap_or(0);
                let site = site_of_domain(&key.0);
                (key, site, age)
            })
            .collect();
        if all.len() <= MAX_PER_DOMAIN {
            return; // no group can be over a limit
        }
        let mut doomed: HashSet<usize> = HashSet::new();
        for (limit, by_site) in [(MAX_PER_DOMAIN, false), (MAX_PER_SITE, true)] {
            let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
            for (i, (key, site, _)) in all.iter().enumerate() {
                if !doomed.contains(&i) {
                    groups.entry(if by_site { site.as_str() } else { key.0.as_str() }).or_default().push(i);
                }
            }
            for (_, mut members) in groups {
                if members.len() > limit {
                    members.sort_by_key(|&i| all[i].2);
                    doomed.extend(members.iter().take(members.len() - limit));
                }
            }
        }
        let mut alive: Vec<usize> = (0..all.len()).filter(|i| !doomed.contains(i)).collect();
        if alive.len() > MAX_TOTAL {
            alive.sort_by_key(|&i| all[i].2);
            doomed.extend(alive.iter().take(alive.len() - MAX_TOTAL));
        }
        for i in doomed {
            let (d, p, n) = &all[i].0;
            // (an evicted persistent cookie must leave the file too)
            self.dirty |= self.store.get(d, p, n).is_some_and(|c| c.is_persistent());
            self.store.remove(d, p, n);
            self.seq.remove(&all[i].0);
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
    JAR.get_or_init(|| {
        let mut jar = Jar::new();
        if let Some(file) = cookie_file() {
            // A missing file just means starting empty. One that is there but can't be read (too
            // big, not text, no permission) is left exactly as it is: never written over.
            match std::fs::metadata(&file) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Ok(m) if m.len() <= MAX_FILE_BYTES => match std::fs::read_to_string(&file) {
                    Ok(text) => jar.load_text(&text),
                    Err(_) => jar.no_save = true,
                },
                _ => jar.no_save = true,
            }
        }
        Mutex::new(jar)
    })
    .lock()
    .unwrap_or_else(|e| e.into_inner())
}

/// Biggest cookie file read back.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Where persistent cookies live: next to the bookmarks (and, like them, never in a test run
/// unless the test names a folder).
fn cookie_file() -> Option<std::path::PathBuf> {
    crate::bookmarks::data_dir().map(|d| d.join("cookies.tsv"))
}

/// Write the jar to disk if a persistent cookie changed since the last write. Called after a
/// response's cookies have been taken in.
pub fn save_if_changed() {
    let Some(file) = cookie_file() else { return };
    // one writer at a time, from taking the snapshot to the rename: an older snapshot can't land
    // after a newer one, and two threads never share the temporary file
    static WRITING: Mutex<()> = Mutex::new(());
    let _writing = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let text = {
        let mut jar = global();
        if !jar.dirty || jar.no_save {
            return;
        }
        jar.dirty = false;
        jar.to_text()
    };
    let written = (|| {
        let dir = file.parent()?;
        crate::bookmarks::create_private_dir(dir).ok()?;
        let tmp = dir.join(format!("cookies.tsv.{}.tmp", std::process::id()));
        let done = crate::bookmarks::write_private_file(&tmp, text.as_bytes()).is_ok() && std::fs::rename(&tmp, &file).is_ok();
        if !done {
            let _ = std::fs::remove_file(&tmp);
        }
        done.then_some(())
    })();
    if written.is_none() {
        global().dirty = true; // (try again after the next change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn header(jar: &Jar, url: &str) -> String {
        jar.header_for(&u(url), Ctx::SameSite).unwrap_or_default()
    }

    #[test]
    fn persistent_cookies_survive_a_restart_and_session_cookies_do_not() {
        let mut jar = Jar::new();
        let site = u("https://shop.example.com/cart/");
        jar.accept(&site, "keep=1; Max-Age=3600; Path=/; Secure; HttpOnly; SameSite=Strict");
        jar.accept(&site, "wide=2; Max-Age=3600; Domain=example.com; Path=/cart");
        jar.accept(&site, "session=3; Path=/");
        let text = jar.to_text();
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(!text.contains("session"));
        let mut fresh = Jar::new();
        fresh.load_text(&text);
        let h = header(&fresh, "https://shop.example.com/cart/");
        assert!(h.contains("keep=1") && h.contains("wide=2") && !h.contains("session"), "{h}");
        // the host-only cookie stays host-only; the Domain one reaches sibling hosts
        let other = header(&fresh, "https://www.example.com/cart/");
        assert!(other.contains("wide=2") && !other.contains("keep"), "{other}");
        // Secure and SameSite survive
        assert_eq!(header(&fresh, "http://shop.example.com/cart/"), "wide=2");
        assert_eq!(header_in(&fresh, "https://shop.example.com/cart/", Ctx::CrossSiteGet), "wide=2");
    }

    #[test]
    fn ipv6_host_cookies_survive_a_restart() {
        let mut jar = Jar::new();
        jar.accept(&u("http://[::1]:8080/"), "c=1; Max-Age=1000; Path=/");
        let text = jar.to_text();
        assert_eq!(text.lines().count(), 1, "{text}");
        let mut fresh = Jar::new();
        fresh.load_text(&text);
        assert_eq!(header(&fresh, "http://[::1]:8080/"), "c=1");
    }

    #[test]
    fn changes_that_must_reach_the_file_mark_the_jar_dirty() {
        let mut jar = Jar::new();
        let site = u("https://a.example.com/");
        jar.accept(&site, "a=1; Max-Age=3600; Path=/");
        assert!(jar.dirty);
        jar.dirty = false;
        jar.accept(&site, "b=1; Path=/"); // a session cookie alone changes nothing on disk
        assert!(!jar.dirty);
        jar.accept(&site, "a=2; Path=/"); // ...but one that replaces a persistent cookie does
        assert!(jar.dirty && jar.to_text().is_empty());
        jar.dirty = false;
        jar.accept(&site, "a=1; Max-Age=3600; Path=/");
        jar.dirty = false;
        jar.accept(&site, "a=; Max-Age=0; Path=/"); // a deletion
        assert!(jar.dirty && jar.to_text().is_empty());
    }

    #[test]
    fn what_is_written_can_always_be_read_back_and_loading_is_fast() {
        let mut jar = Jar::new();
        // far more than the file may hold: 30 hosts of 50 cookies of 3.5 KB
        for h in 0..30 {
            let site = u(&format!("https://h{h}.example{h}.com/"));
            for n in 0..50 {
                jar.accept(&site, &format!("c{n}={}; Max-Age=3600; Path=/", "v".repeat(3500)));
            }
        }
        let text = jar.to_text();
        assert!((text.len() as u64) <= MAX_FILE_BYTES, "{} bytes", text.len());
        let started = std::time::Instant::now();
        let mut fresh = Jar::new();
        fresh.load_text(&text);
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "loading took {:?}", started.elapsed());
        assert!(fresh.len() > 100);
        // a value with `;` in a hand-edited file can't add attributes
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 3600;
        let mut edited = Jar::new();
        edited.load_text(&format!("https\t1\tsub.example.com\t/\ta\t1; Domain=example.com\t{now}\t0\t0\t\n"));
        assert_eq!(header(&edited, "https://www.example.com/"), "");
        assert_eq!(edited.len(), 0);
    }

    #[test]
    fn a_damaged_or_edited_cookie_file_is_checked_like_any_response() {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let later = now + 3600;
        let text = format!(
            "https\t0\tco.uk\t/\tsuper\t1\t{later}\t0\t0\t\n\
             http\t1\texample.com\t/\told\t1\t{}\t0\t0\t\n\
             http\t1\texample.com\t/\tinsecure\t1\t{later}\t1\t0\t\n\
             http\t1\texample.com\t/\tgood\t1\t{later}\t0\t0\tLax\n\
             http\t1\texample.com\t/\ttoo\tfew\n\
             ftp\t1\texample.com\t/\tweird\t1\t{later}\t0\t0\t\n\
             http\t1\tevil.com/x@\t/\thost\t1\t{later}\t0\t0\t\n\
             garbage\n",
            now.saturating_sub(10)
        );
        let mut jar = Jar::new();
        jar.load_text(&text);
        assert_eq!(header(&jar, "http://example.com/"), "good=1");
        assert_eq!(header(&jar, "http://foo.co.uk/"), "");
    }

    fn header_in(jar: &Jar, url: &str, ctx: Ctx) -> String {
        jar.header_for(&u(url), ctx).unwrap_or_default()
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
    fn the_cookie_header_order_is_stable_longer_paths_then_oldest_first() {
        // the order used to depend on hash-map iteration, so it changed from run to run
        for _ in 0..20 {
            let mut jar = Jar::new();
            let url = u("https://example.com/a/b/c");
            for set in ["first=1; Path=/", "deep=2; Path=/a/b", "mid=3; Path=/a", "second=4; Path=/"] {
                assert!(jar.accept(&url, set));
            }
            assert_eq!(header(&jar, "https://example.com/a/b/c"), "deep=2; mid=3; first=1; second=4");
        }
    }

    #[test]
    fn samesite_decides_which_cookies_cross_sites() {
        let mut jar = Jar::new();
        let url = u("https://example.com/");
        for set in [
            "strict=1; SameSite=Strict; Path=/",
            "lax=2; SameSite=Lax; Path=/",
            "plain=3; Path=/", // no attribute: behaves as Lax
            "none=4; SameSite=None; Secure; Path=/",
        ] {
            assert!(jar.accept(&url, set), "{set}");
        }
        assert_eq!(header_in(&jar, "https://example.com/", Ctx::SameSite), "strict=1; lax=2; plain=3; none=4");
        // a link from another site: everything but Strict
        assert_eq!(header_in(&jar, "https://example.com/", Ctx::CrossSiteGet), "lax=2; plain=3; none=4");
        // a form POST from another site (CSRF): only SameSite=None
        assert_eq!(header_in(&jar, "https://example.com/", Ctx::CrossSitePost), "none=4");
        // SameSite=None without Secure is refused outright
        assert!(!jar.accept(&url, "bad=1; SameSite=None; Path=/"));
    }

    #[test]
    fn request_context_depends_on_who_started_it() {
        let page = u("https://shop.example.com/cart");
        let same = u("https://api.example.com/pay");
        let other = u("https://evil.test/");
        assert_eq!(context(None, &same, true), Ctx::SameSite, "typed addresses and history are not cross-site");
        assert_eq!(context(Some(&page), &same, true), Ctx::SameSite, "subdomains are the same site");
        assert_eq!(context(Some(&page), &other, false), Ctx::CrossSiteGet);
        assert_eq!(context(Some(&page), &other, true), Ctx::CrossSitePost);
        assert_eq!(context(Some(&u("file:///a.html")), &other, false), Ctx::CrossSiteGet);
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
    fn plain_http_cannot_overwrite_or_delete_a_secure_cookie() {
        // a network attacker on any http page of the host used to plant values in https sessions
        let mut jar = Jar::new();
        assert!(jar.accept(&u("https://example.com/"), "tok=SECURE; Secure; Path=/"));
        assert!(!jar.accept(&u("http://example.com/"), "tok=EVIL; Path=/"));
        assert!(!jar.accept(&u("http://example.com/"), "tok=; Path=/; Max-Age=0"), "nor delete it");
        assert_eq!(header(&jar, "https://example.com/"), "tok=SECURE");
        // https itself may still update it
        assert!(jar.accept(&u("https://example.com/"), "tok=NEW; Secure; Path=/"));
        assert_eq!(header(&jar, "https://example.com/"), "tok=NEW");
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
        // the oldest 150 were evicted: what is sent starts with the oldest survivor
        assert!(h.starts_with("c150="), "{}", &h[..h.len().min(40)]);
        assert!(!h.contains("c0=") && !h.contains("c149="));
        // other domains are untouched and still work
        assert!(jar.accept(&u("https://other.example.org/"), "fine=1"));
        assert_eq!(header(&jar, "https://other.example.org/"), "fine=1");
    }

    #[test]
    fn one_site_flooding_cookies_cannot_evict_another_sites() {
        // wildcard DNS gave an attacker a fresh 50-cookie bucket per subdomain, and the
        // global cap then dropped arbitrary cookies, including the bank's session
        let mut jar = Jar::new();
        assert!(jar.accept(&u("https://bank.com/"), "session=BANK; Path=/; Secure"));
        for host in 0..70 {
            for c in 0..50 {
                jar.accept(&u(&format!("https://h{host}.attacker.com/")), &format!("c{c}=1; Path=/"));
            }
        }
        assert_eq!(header(&jar, "https://bank.com/"), "session=BANK");
        assert!(jar.len() <= MAX_PER_SITE + 1, "{} cookies kept", jar.len());
        // a sibling subdomain flooding Domain=example.com cookies stays within the site's cap
        let mut jar = Jar::new();
        assert!(jar.accept(&u("https://www.example.com/"), "sid=GOOD; Path=/"));
        for i in 0..400 {
            jar.accept(&u("https://evil.example.com/"), &format!("f{i}=1; Domain=example.com; Path=/"));
        }
        assert!(jar.len() <= MAX_PER_SITE, "{} cookies kept", jar.len());
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
