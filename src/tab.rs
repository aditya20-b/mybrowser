//! One browser tab: its page, history, scroll position and the navigation in
//! flight. Everything here is independent of the window system (loaders report
//! back through a plain callback), so it is unit-tested without opening one.
use crate::dom::NodeId;
use crate::download::{self, PendingDownload};
use crate::forms::{self, ControlKind, FormState};
use crate::net;
use crate::page::Page;
use std::collections::HashMap;
use std::sync::Arc;
use url::Url;

/// What a clicked `href` means.
#[derive(Debug, PartialEq)]
pub enum Link {
    /// Nothing to do: empty href, or an unparseable URL.
    Nothing,
    /// A scheme we don't handle (`mailto:`, `javascript:`, `tel:`, ...) or
    /// something a page isn't allowed to open (a web page linking to `file:`).
    External(String),
    Navigate(Url),
    /// A link to a `#fragment` of the page that is already open: scroll, don't load.
    /// (The fragment is still percent-encoded; empty for a bare `#`.)
    Fragment(String),
}

fn strip_fragment(u: &Url) -> Url {
    let mut u = u.clone();
    u.set_fragment(None);
    u
}

/// Is `host` (no port; an IPv6 address may still carry its brackets) exactly
/// `localhost` (a trailing dot is fine) or a loopback IP address? Prefix matching
/// would treat `localhost.evil.com` or `127.evil.com` as local and send the visit
/// to a remote server in cleartext.
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').trim_end_matches('.');
    h.eq_ignore_ascii_case("localhost") || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// What `href` means on a page at `doc` whose relative URLs resolve against `base`
/// (they differ when the page has a `<base href>`). A link that lands on a
/// fragment of the page itself doesn't reload it; one to the page's own URL does.
pub fn resolve_link_in(doc: &Url, base: &Url, href: &str) -> Link {
    let href = href.trim();
    if href.is_empty() {
        return Link::Nothing;
    }
    if base == doc {
        if let Some(frag) = href.strip_prefix('#') {
            return Link::Fragment(frag.to_string());
        }
    }
    let Ok(target) = base.join(href) else { return Link::Nothing };
    match target.scheme() {
        // web pages must not be able to send you to local files
        "file" if matches!(doc.scheme(), "http" | "https") => Link::External(target.to_string()),
        "http" | "https" | "file" => {
            if strip_fragment(&target) == strip_fragment(doc) {
                // same document with a fragment: no reload. Without one the link asks for the
                // page again, which is a (replacing) navigation, as in other browsers.
                match target.fragment() {
                    Some(f) => Link::Fragment(f.to_string()),
                    None => Link::Navigate(target),
                }
            } else {
                Link::Navigate(target)
            }
        }
        _ => Link::External(target.to_string()),
    }
}

/// `resolve_link_in` for a page without a `<base href>`.
pub fn resolve_link(base: &Url, href: &str) -> Link {
    resolve_link_in(base, base, href)
}

/// May a form on `from` send its data to `target`? Data isn't sent from an https
/// page to plain http (it would travel in the clear, passwords included, unless
/// the server is on this machine), and only http(s) servers can receive a POST.
pub fn form_target_allowed(from: &Url, target: &Url, post: bool) -> Result<(), String> {
    if post && !matches!(target.scheme(), "http" | "https") {
        return Err(format!("can't submit a form to {}: URLs", target.scheme()));
    }
    let local = target.host_str().is_some_and(is_loopback_host);
    if from.scheme() == "https" && target.scheme() == "http" && !local {
        return Err("blocked: this secure page tried to send form data over plain http".into());
    }
    Ok(())
}

/// A fully built page on its way from a loader thread to the UI thread.
///
/// `Page` holds `Rc<Image>`s, which are not `Send`. That is only a problem if
/// two threads could touch the same reference count, and here they can't: every
/// clone of every `Rc` lives inside this one `Page` (the loader's temporary
/// caches are dropped before it returns), and the loader thread gives the whole
/// value away through a channel without keeping anything. Ownership moves as a
/// unit, so the counts are only ever touched by one thread at a time.
pub struct SendPage(pub Page);
unsafe impl Send for SendPage {}

pub enum UserEvent {
    Loaded { tab: u64, ticket: u64, page: SendPage },
}

/// How a loader thread reports a finished page to the UI thread.
pub type Notify = Arc<dyn Fn(UserEvent) + Send + Sync>;

/// How a finished navigation changes the history stacks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hist {
    /// A new page: the one we leave goes on the back stack, forward is dropped.
    Push,
    /// Back button: step one entry back.
    Back,
    /// Forward button: step one entry forward.
    Forward,
    /// Reload: history doesn't change.
    Replace,
}

/// The back and forward stacks. They only change when a navigation *lands*, never
/// when it starts, so a load that is cancelled or superseded can't lose or
/// duplicate entries.
#[derive(Default, Debug, PartialEq)]
pub struct History {
    pub back: Vec<Url>,
    pub forward: Vec<Url>,
}

impl History {
    /// The page the Back / Forward button would go to.
    pub fn back_target(&self) -> Option<&Url> {
        self.back.last()
    }

    pub fn forward_target(&self) -> Option<&Url> {
        self.forward.last()
    }

    /// A navigation finished: `from` is the page that was on screen, `landed` the
    /// page that replaced it (after any redirects).
    pub fn apply(&mut self, how: Hist, from: Url, landed: &Url) {
        match how {
            Hist::Push => {
                // Landing on the very page we left (a redirect back to it, or a link to
                // the current URL) is a replace, not a new entry.
                if &from != landed {
                    self.back.push(from);
                    self.forward.clear();
                }
            }
            Hist::Back => {
                self.back.pop();
                self.forward.push(from);
            }
            Hist::Forward => {
                self.forward.pop();
                self.back.push(from);
            }
            Hist::Replace => {}
        }
    }
}

/// A navigation that has started but not finished.
#[derive(Debug, PartialEq)]
pub struct Pending {
    pub ticket: u64,
    /// What is being loaded.
    pub url: Url,
    pub hist: Hist,
    /// The page on screen when this started (the one a successful load replaces).
    pub from: Url,
    /// Restore this scroll offset when it lands (reload keeps your place).
    pub keep_scroll: Option<f32>,
}

/// Bookkeeping for the one navigation in flight. Each start gets a ticket;
/// only the newest ticket may complete, so a slow, superseded load can never
/// replace a page the user has since moved on to.
#[derive(Default)]
pub struct Navigation {
    ticket: u64,
    pending: Option<Pending>,
}

impl Navigation {
    pub fn start(&mut self, from: Url, url: Url, hist: Hist, keep_scroll: Option<f32>) -> u64 {
        self.ticket += 1;
        self.pending = Some(Pending { ticket: self.ticket, url, hist, from, keep_scroll });
        self.ticket
    }

    /// A load finished: returns its details if it is still the current one.
    pub fn finish(&mut self, ticket: u64) -> Option<Pending> {
        if ticket == self.ticket { self.pending.take() } else { None }
    }

    /// Abandon whatever is loading. Returns whether something was.
    pub fn cancel(&mut self) -> bool {
        self.ticket += 1;
        self.pending.take().is_some()
    }

    pub fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    pub fn is_loading(&self) -> bool {
        self.pending.is_some()
    }
}

/// The address a brand-new tab shows.
pub const BLANK_URL: &str = "about:blank";

pub fn blank_url() -> Url {
    Url::parse(BLANK_URL).expect("a constant, valid URL")
}

fn blank_page(url: &Url) -> Page {
    Page::from_source(
        url,
        "<title>New Tab</title><body style='background:#f8f9fa;color:#5f6368'><p style='margin-top:140px;text-align:center'>New tab</p>",
    )
}

pub struct Tab {
    pub id: u64,
    pub page: Page,
    pub history: History,
    pub nav: Navigation,
    pub scroll: f32,
    /// The viewport width the page was last laid out for (`-1` forces a relayout).
    pub laid_out_width: f32,
    /// `page` is just the "Loading…" stand-in of a tab opened straight onto a URL,
    /// not something that was ever loaded.
    placeholder: bool,
    /// A `#fragment` of a freshly loaded page to scroll to once it has been laid out.
    pending_fragment: Option<String>,
    /// Form data for the next `begin_load` to POST (set by `submit_form`).
    next_post: Option<String>,
    /// Form data of the navigation in flight, so restarting it (reload while
    /// loading) POSTs again instead of silently turning into a GET.
    inflight_post: Option<String>,
    /// The page the next `begin_load` was started from (a clicked link or a
    /// submitted form); `None` for typed addresses and history. Decides `SameSite` cookies.
    next_initiator: Option<Url>,
    inflight_initiator: Option<Url>,
    /// Where each page left behind in this tab was scrolled to (by full URL, fragment included).
    scroll_memory: HashMap<(Url, usize), f32>,
    /// Pages recently left, by (address, depth in the back history), for instant Back / Forward.
    page_cache: std::collections::VecDeque<CachedPage>,
    /// How many meta-refresh redirects led to the page on screen, and to the load in flight.
    /// What happened to the last file the tab was asked to download (shown once).
    notice: Option<String>,
    /// How many history entries behind / ahead of the page on screen are fragment positions of
    /// this same document (Back and Forward between them need no load).
    frag_back: usize,
    frag_forward: usize,
    /// The page on screen is the note for a file this tab downloaded (not a page to go back to).
    on_download_page: bool,
    /// The tab was brought back with Cmd+Shift+T: its first load never saves a file.
    restored: bool,
    refresh_hops: u8,
    inflight_hops: u8,
    /// Set just before `begin_load` by a meta refresh; taken by it.
    next_hops: u8,
    notify: Notify,
}

/// A chain of zero-delay meta refreshes is cut off after this many.
const MAX_REFRESH_HOPS: u8 = 5;

/// A tab keeps this many recently left pages in memory for Back and Forward...
const MAX_CACHED_PAGES: usize = 6;
/// ...as long as their decoded images take no more than this much in all.
const MAX_CACHED_IMAGE_BYTES: u64 = 96 * 1024 * 1024;

/// A page held for Back / Forward.
struct CachedPage {
    /// (address, depth in the back history when it was left)
    key: (Url, usize),
    /// The fragment-entry counts around it then (see `Tab::frag_back`).
    frags: (usize, usize),
    page: Page,
}

/// A tab remembers the scroll position of at most this many pages.
const MAX_REMEMBERED_SCROLLS: usize = 200;

impl Tab {
    /// A new tab that starts loading `start` (started from `initiator`, if a page opened it).
    pub fn new(id: u64, start: Url, initiator: Option<Url>, notify: Notify) -> Tab {
        let mut tab = Tab::blank(id, notify);
        tab.page = Page::from_source(&start, "<p style='color:#666'>Loading…</p>");
        tab.placeholder = true;
        tab.next_initiator = initiator;
        tab.begin_load(start, Hist::Replace, None);
        tab
    }

    /// An empty tab (nothing loading).
    pub fn blank(id: u64, notify: Notify) -> Tab {
        Tab {
            id,
            page: blank_page(&blank_url()),
            history: History::default(),
            nav: Navigation::default(),
            scroll: 0.0,
            laid_out_width: -1.0,
            placeholder: false,
            pending_fragment: None,
            next_post: None,
            inflight_post: None,
            next_initiator: None,
            inflight_initiator: None,
            scroll_memory: HashMap::new(),
            page_cache: std::collections::VecDeque::new(),
            notice: None,
            on_download_page: false,
            frag_back: 0,
            frag_forward: 0,
            restored: false,
            refresh_hops: 0,
            inflight_hops: 0,
            next_hops: 0,
            notify,
        }
    }

    pub fn is_loading(&self) -> bool {
        self.nav.is_loading()
    }

    /// What the address bar should show: where we are going, else where we are
    /// (nothing for a blank tab).
    pub fn address(&self) -> String {
        match self.nav.pending() {
            Some(p) => p.url.to_string(),
            None if self.page.url.scheme() == "about" => String::new(),
            None => self.page.url.to_string(),
        }
    }

    /// The tab's label: the page title, or "New Tab" for a blank one.
    pub fn title(&self) -> String {
        if self.page.url.scheme() == "about" && !self.is_loading() { "New Tab".into() } else { self.page.display_title() }
    }

    /// Start loading `url` on a background thread. The current page stays on
    /// screen and responsive until the new one arrives; starting another load (or
    /// cancelling) abandons this one. The history stacks are only touched when the
    /// load lands (see `finish_load`). If `next_post` is set, the load is a form POST
    /// with that body; `next_initiator` names the page it came from.
    pub fn begin_load(&mut self, url: Url, hist: Hist, keep_scroll: Option<f32>) {
        let ticket = self.nav.start(self.page.url.clone(), url.clone(), hist, keep_scroll);
        self.inflight_hops = std::mem::take(&mut self.next_hops);
        self.notice = None; // a note about an earlier download isn't about this page
        let post = self.next_post.take();
        let initiator = self.next_initiator.take();
        self.inflight_post = post.clone();
        self.inflight_initiator = initiator.clone();
        let (notify, tab) = (self.notify.clone(), self.id);
        std::thread::spawn(move || {
            let load = || {
                if url.scheme() == "about" {
                    return blank_page(&url);
                }
                net::with_initiator(initiator.clone(), || match &post {
                    Some(body) => Page::load_post(&url, body),
                    None => Page::load(&url),
                })
            };
            // A panic while loading must still end the "Loading…" state, even if
            // building the error page itself panics.
            let page = std::panic::catch_unwind(std::panic::AssertUnwindSafe(load))
                .or_else(|_| std::panic::catch_unwind(|| Page::error(&url, "internal error while loading this page")))
                .unwrap_or_else(|_| Page::from_source(&url, "<p>Could not load this page.</p>"));
            notify(UserEvent::Loaded { tab, ticket, page: SendPage(page) });
        });
    }

    /// A loader thread delivered a page. Returns whether it was applied (a
    /// superseded or cancelled load is ignored).
    pub fn finish_load(&mut self, ticket: u64, mut page: Page) -> bool {
        let Some(pending) = self.nav.finish(ticket) else { return false };
        let was_post = self.inflight_post.take().is_some();
        self.inflight_initiator = None;
        if let Some(download) = page.download.take() {
            return self.land_download(pending, page, download, was_post);
        }
        // leaving the note page of a download (a tab opened onto a file) leaves nothing behind in history
        // a new document: the fragment entries before it belong to the old one (kept with the
        // old page if it goes into the cache, so a later restore can pick them up again)
        let left_frags = (self.frag_back, self.frag_forward);
        (self.frag_back, self.frag_forward) = (0, 0);
        let leaving_note = std::mem::take(&mut self.on_download_page);
        let pending = if leaving_note && pending.hist == Hist::Push { Pending { hist: Hist::Replace, ..pending } } else { pending };
        // `page.html#section`: jump there once the page has been laid out. A redirect that
        // names no fragment of its own keeps the one that was asked for, as in other browsers.
        // A reload that remembers where you were stays put instead.
        self.pending_fragment = match pending.keep_scroll {
            None => page.url.fragment().or(pending.url.fragment()).map(String::from),
            Some(_) => None,
        };
        // remember where the page being left was scrolled to, so Back / Forward can return there
        if !self.placeholder {
            self.remember_scroll(pending.from.clone());
        }
        let leave_depth = self.history.back.len();
        let was_placeholder = self.placeholder;
        self.history.apply(pending.hist, pending.from, &page.url);
        // (positions are kept per place in the history, so visiting a URL twice keeps two)
        let depth = self.history.back.len();
        let restored = match (pending.hist, pending.keep_scroll) {
            (Hist::Back | Hist::Forward, None) => {
                self.scroll_memory.get(&(pending.url.clone(), depth)).or(self.scroll_memory.get(&(page.url.clone(), depth))).copied()
            }
            _ => None,
        };
        if restored.is_some() {
            self.pending_fragment = None;
        }
        let old = std::mem::replace(&mut self.page, page);
        // keep the page just left, whole (typed form data included), for Back and Forward; a
        // reload or a landing on the same address replaces it instead
        if !was_placeholder && pending.hist != Hist::Replace && old.url != self.page.url {
            if pending.hist == Hist::Push {
                self.page_cache.retain(|c| c.key.1 <= leave_depth); // the forward entries are gone
            }
            self.cache_page(old, leave_depth, left_frags);
        }
        // the page now shown must not also sit in the cache (a copy kept from an earlier visit
        // would otherwise come back later in its place)
        let shown = (self.page.url.clone(), depth);
        self.page_cache.retain(|c| c.key != shown);
        self.placeholder = false;
        self.laid_out_width = -1.0; // force relayout on the next redraw
        self.scroll = restored.or(pending.keep_scroll).unwrap_or(0.0); // clamped on the next redraw
        self.refresh_hops = std::mem::take(&mut self.inflight_hops);
        self.follow_meta_refresh();
        true
    }

    /// A load landed on a file we can't display. The page you were on stays (a download is not
    /// a navigation, so history and scroll are untouched) and the file is saved only if the
    /// user asked for it: a link, address or form they opened, never a reload, Back, Forward or
    /// a meta refresh (that would be a drive-by download, or one copy more each time). A load
    /// that was cancelled or superseded never gets here, so it writes nothing either.
    /// The outcome goes to `notice`, for the window to show. A tab that has shown nothing yet
    /// (opened straight onto the file) shows the note page instead.
    fn land_download(&mut self, pending: Pending, page: Page, download: PendingDownload, was_post: bool) -> bool {
        let hops = std::mem::take(&mut self.inflight_hops);
        // (a tab opened straight onto an address has shown nothing yet: that load is the user's too,
        // unless the tab was only brought back from the closed list; a form the user submitted
        // counts even when it posts to the page's own address, which is a "replace" load)
        let first_load = self.placeholder && !self.restored;
        let asked = (pending.hist == Hist::Push || first_load || was_post) && pending.keep_scroll.is_none() && hops == 0;
        let name = download::display_safe(&download.name, 120);
        self.notice = Some(if !asked {
            format!("Not saved: {name} (a download only starts from a link or address you open)")
        } else {
            match download::downloads_dir() {
                None => format!("Cannot save {name}: there is no downloads folder"),
                Some(dir) => match download::save(&dir, &download.name, &download.body) {
                    Ok(path) => format!(
                        "Saved {name} ({}) to {}",
                        download::display_safe(&download.content_type, 100),
                        download::display_safe(&path.to_string_lossy(), 300)
                    ),
                    Err(e) => format!("Cannot save {name}: {}", download::display_safe(&e.to_string(), 200)),
                },
            }
        });
        if self.placeholder {
            // the note page is all this tab has; leaving it later must not put its address in the history
            self.page = page;
            self.placeholder = false;
            self.on_download_page = true;
            self.laid_out_width = -1.0;
            self.scroll = 0.0;
        }
        true
    }

    /// Mark a tab that was brought back from the closed-tabs list: whatever it loads first is
    /// not something the user just asked for, so a file there is never saved.
    pub fn mark_restored(&mut self) {
        self.restored = true;
    }

    /// The message about the last download, once (the window shows it in the address bar).
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    /// A page that says `<meta http-equiv=refresh content="0; url=...">` sends the tab on to
    /// that address at once (replacing the page in the history). Only zero-delay refreshes
    /// are followed (there are no timers), at most `MAX_REFRESH_HOPS` in a row, and only to
    /// places an ordinary link could go.
    fn follow_meta_refresh(&mut self) {
        let Some((delay, href)) = self.page.meta_refresh() else { return };
        if delay > 0.0 || self.refresh_hops >= MAX_REFRESH_HOPS {
            return;
        }
        if let Link::Navigate(url) = self.classify(&href) {
            if url != self.page.url {
                self.next_hops = self.refresh_hops + 1;
                // the page asked for this, so it is the initiator: SameSite cookies must treat
                // a cross-site refresh like a cross-site link, not like a typed address
                self.next_initiator = Some(self.page.url.clone());
                self.begin_load(url, Hist::Replace, None);
            }
        }
    }

    /// Note the current scroll position as the one for `url`, the page on screen at this
    /// depth of the back history (call it as the page is left, before history changes).
    fn remember_scroll(&mut self, url: Url) {
        if self.scroll_memory.len() >= MAX_REMEMBERED_SCROLLS {
            // forget the oldest quarter (the shallowest places in the history), not everything
            let mut depths: Vec<usize> = self.scroll_memory.keys().map(|(_, d)| *d).collect();
            depths.sort_unstable();
            let cut = depths[depths.len() / 4];
            self.scroll_memory.retain(|(_, d), _| *d > cut);
        }
        let depth = self.history.back.len();
        self.scroll_memory.insert((url, depth), self.scroll);
    }

    /// Scroll to the target of `fragment` in the current layout. Returns whether
    /// anything matched (otherwise the page stays where it is).
    pub fn scroll_to_fragment(&mut self, fragment: &str) -> bool {
        // A target (the first element with that id, as in browsers) inside a closed <details>
        // opens it first, then we lay out again and scroll once that has happened. (Checked
        // before looking for a laid-out target, so a visible duplicate id can't win.)
        if self.page.reveal_fragment(fragment) {
            self.pending_fragment = Some(fragment.to_string());
            self.laid_out_width = -1.0;
            return true;
        }
        match self.page.fragment_y(fragment) {
            Some(y) => {
                self.scroll = y.max(0.0); // clamped to the page on the next redraw
                true
            }
            None => false,
        }
    }

    /// Call after the page has been laid out: applies the fragment of a page that
    /// was just loaded (once; a stale layout, `laid_out_width < 0`, waits).
    pub fn settle_fragment(&mut self) {
        if self.laid_out_width < 0.0 {
            return;
        }
        if let Some(f) = self.pending_fragment.take() {
            self.scroll_to_fragment(&f);
        }
    }

    /// Give up on the load in flight. Returns whether there was one. A tab that was
    /// opened straight onto a URL and has shown nothing yet goes back to being a blank tab.
    pub fn cancel_load(&mut self) -> bool {
        let was = self.nav.cancel();
        if was {
            self.inflight_post = None;
            self.inflight_initiator = None;
            if self.placeholder {
                self.page = blank_page(&blank_url());
                self.placeholder = false;
                self.laid_out_width = -1.0;
            }
        }
        was
    }

    pub fn open(&mut self, url: Url) {
        self.begin_load(url, Hist::Push, None);
    }

    /// Reload. While a navigation is loading, that navigation is restarted (not
    /// silently replaced by a reload of the page underneath it), form data
    /// included. Reloading a page that a POST produced fetches its URL with a GET.
    pub fn reload(&mut self) {
        if let Some(p) = self.nav.pending() {
            let (url, hist) = (p.url.clone(), p.hist);
            self.next_post = self.inflight_post.clone();
            self.next_initiator = self.inflight_initiator.clone();
            self.begin_load(url, hist, None);
        } else if self.page.url.scheme() != "about" {
            let (url, keep) = (self.page.url.clone(), self.scroll);
            self.begin_load(url, Hist::Replace, Some(keep));
        }
    }

    /// Back. Returns whether a different page was swapped in on the spot (from memory), so the
    /// window knows anything tied to the old one (a find in progress) is stale.
    pub fn go_back(&mut self) -> bool {
        if let Some(prev) = self.history.back_target().cloned() {
            if self.same_document_move(&prev, true) {
                return false;
            }
            if self.restore_cached(&prev, true) {
                return true;
            }
            self.begin_load(prev, Hist::Back, None);
        }
        false
    }

    /// Forward; see `go_back`.
    pub fn go_forward(&mut self) -> bool {
        if let Some(next) = self.history.forward_target().cloned() {
            if self.same_document_move(&next, false) {
                return false;
            }
            if self.restore_cached(&next, false) {
                return true;
            }
            self.begin_load(next, Hist::Forward, None);
        }
        false
    }

    /// Remember a page that was just left, at this depth of the back history (with the
    /// fragment-entry counts around it). Pages that failed to load are not kept (Back should try
    /// again), and a page is only kept while the pages held stay within a memory budget.
    fn cache_page(&mut self, page: Page, depth: usize, frags: (usize, usize)) {
        if page.url.scheme() == "about"
            || page.is_error
            || page.no_store
            || page.status.is_some_and(|s| s >= 400)
            || page.image_bytes() > MAX_CACHED_IMAGE_BYTES
        {
            return;
        }
        let key = (page.url.clone(), depth);
        self.page_cache.retain(|c| c.key != key);
        self.page_cache.push_back(CachedPage { key, frags, page });
        let total = |cache: &std::collections::VecDeque<CachedPage>| cache.iter().map(|c| c.page.image_bytes()).sum::<u64>();
        while self.page_cache.len() > MAX_CACHED_PAGES || total(&self.page_cache) > MAX_CACHED_IMAGE_BYTES {
            self.page_cache.pop_front();
        }
    }

    /// Back or Forward onto a page we still hold: swap it in, with what was typed into it and
    /// where it was scrolled, without touching the network (so a page a POST produced comes
    /// back as it was, not re-fetched with a GET). Not while a load is in flight.
    fn restore_cached(&mut self, target: &Url, back: bool) -> bool {
        if self.nav.pending().is_some() || self.placeholder {
            return false;
        }
        let depth_after = if back { self.history.back.len().saturating_sub(1) } else { self.history.back.len() + 1 };
        let Some(pos) = self.page_cache.iter().position(|c| c.key.0 == *target && c.key.1 == depth_after) else { return false };
        let CachedPage { page: cached, frags, .. } = self.page_cache.remove(pos).unwrap();
        let here = self.page.url.clone();
        self.remember_scroll(here.clone());
        let leave_depth = self.history.back.len();
        let leaving_frags = (self.frag_back, self.frag_forward);
        let old = std::mem::replace(&mut self.page, cached);
        self.cache_page(old, leave_depth, leaving_frags);
        if back {
            self.history.back.pop();
            self.history.forward.push(here);
        } else {
            self.history.forward.pop();
            self.history.back.push(here);
        }
        self.on_download_page = false;
        // the fragment entries around the page are as they were when it was left
        (self.frag_back, self.frag_forward) = frags;
        self.pending_fragment = None;
        self.laid_out_width = -1.0;
        self.scroll = self.scroll_memory.get(&(target.clone(), depth_after)).copied().unwrap_or(0.0);
        true
    }

    /// Back / forward between two `#fragment`s of the page on screen needs no load: just
    /// move along the history and scroll. Returns whether it did (not while a load is in flight).
    fn same_document_move(&mut self, target: &Url, back: bool) -> bool {
        // only between entries that fragment clicks made on this very document: an entry with the
        // same address from an earlier load (a form's result, a reload) is a different page
        let steps_left = if back { self.frag_back } else { self.frag_forward };
        if steps_left == 0 || self.nav.pending().is_some() || strip_fragment(target) != strip_fragment(&self.page.url) {
            return false;
        }
        let here = std::mem::replace(&mut self.page.url, target.clone());
        self.remember_scroll(here.clone());
        if back {
            self.history.back.pop();
            self.history.forward.push(here);
            (self.frag_back, self.frag_forward) = (self.frag_back - 1, self.frag_forward + 1);
        } else {
            self.history.forward.pop();
            self.history.back.push(here);
            (self.frag_back, self.frag_forward) = (self.frag_back + 1, self.frag_forward - 1);
        }
        let depth = self.history.back.len();
        match (self.scroll_memory.get(&(target.clone(), depth)).copied(), target.fragment()) {
            (Some(y), _) => self.scroll = y,
            (None, Some(f)) => {
                self.scroll_to_fragment(f);
            }
            (None, None) => self.scroll = 0.0,
        }
        true
    }

    /// What a link with this `href` on the current page means (relative links resolve
    /// against the page's `<base href>`, if it has one).
    fn classify(&self, href: &str) -> Link {
        resolve_link_in(&self.page.url, &self.page.base, href)
    }

    /// Follow a link on this page. Returns whether a navigation started (a link to
    /// a `#fragment` of this page just scrolls, and returns false).
    pub fn follow(&mut self, href: &str) -> bool {
        match self.classify(href) {
            Link::Nothing => false,
            Link::Fragment(f) => {
                // the address gains the fragment, with a history entry of its own
                // (not while a load is in flight: that would be a lie about the page)
                if self.nav.pending().is_none() {
                    let mut url = strip_fragment(&self.page.url);
                    url.set_fragment(Some(&f));
                    if url != self.page.url {
                        let here = std::mem::replace(&mut self.page.url, url);
                        self.remember_scroll(here.clone());
                        self.history.back.push(here);
                        self.history.forward.clear();
                        (self.frag_back, self.frag_forward) = (self.frag_back + 1, 0);
                    }
                }
                self.scroll_to_fragment(&f);
                false
            }
            Link::External(url) => {
                eprintln!("not supported (yet): {url}");
                false
            }
            Link::Navigate(url) => {
                self.next_initiator = Some(self.page.url.clone());
                self.open(url);
                true
            }
        }
    }

    /// Where a link clicked on this page would lead, if it leads somewhere we can open.
    pub fn link_target(&self, href: &str) -> Option<Url> {
        match self.classify(href) {
            Link::Navigate(url) => Some(url),
            _ => None,
        }
    }

    pub fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let l = self.page.layout.links.iter().find(|l| x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h);
        l.map(|l| l.href.clone())
    }

    /// The form state changed (focus, text, a toggle): the page must be laid out again.
    pub fn form_changed(&mut self) {
        self.laid_out_width = -1.0;
    }

    /// A click landed on control `id`. An `Err` is a message for the address bar
    /// (a submission that was refused).
    pub fn activate_control(&mut self, id: NodeId) -> Result<(), String> {
        if forms::is_disabled(&self.page.dom, id) && self.page.dom.tag(id) != Some("summary") {
            return Ok(());
        }
        // a click on a <details> heading opens or closes it
        {
            let Page { dom, form, .. } = &mut self.page;
            if form.toggle_details(dom, id) {
                self.form_changed();
                return Ok(());
            }
        }
        let kind = forms::kind_of(&self.page.dom, id);
        let mut result = Ok(());
        {
            let Page { dom, form, .. } = &mut self.page;
            match kind {
                Some(ControlKind::Text | ControlKind::Password | ControlKind::Textarea | ControlKind::Button) => {
                    // clicking into a field already being edited leaves its caret alone; arriving puts it at the end
                    if form.focus != Some(id) {
                        form.move_caret(dom, id, forms::CaretMove::End);
                    }
                    form.focus = Some(id);
                }
                Some(ControlKind::Checkbox | ControlKind::Radio) => {
                    form.focus = Some(id);
                    form.toggle(dom, id);
                }
                Some(ControlKind::Select) => {
                    form.focus = Some(id);
                    form.cycle_select(dom, id);
                }
                Some(ControlKind::Submit) => form.focus = Some(id),
                _ => {}
            }
        }
        if kind == Some(ControlKind::Button) && FormState::is_reset_button(&self.page.dom, id) {
            if let Some(form_el) = forms::form_of(&self.page.dom, id) {
                let Page { dom, form, .. } = &mut self.page;
                form.reset_form(dom, form_el);
            }
        }
        if kind == Some(ControlKind::Submit) {
            result = self.submit_form(id, Some(id));
        }
        self.form_changed();
        result
    }

    /// Submit the form that contains `control`, as if `submitter` (a button) was clicked.
    /// The target goes through the same safety checks as a link (a web page's form
    /// can't send you to `file:` or `javascript:`), plus `form_target_allowed`. An `Err`
    /// is a message for the address bar.
    pub fn submit_form(&mut self, control: NodeId, submitter: Option<NodeId>) -> Result<(), String> {
        let Some(form_el) = forms::form_of(&self.page.dom, control) else { return Ok(()) };
        // An action written in the page resolves against the page's `<base href>`; a form with
        // no action at all submits to the document's own address.
        let dom = &self.page.dom;
        let written = |el: NodeId, attr: &str| dom.attr(el, attr).is_some_and(|v| !v.trim().is_empty());
        let action_in_markup = written(form_el, "action") || submitter.is_some_and(|s| written(s, "formaction"));
        let resolve_against = if action_in_markup { self.page.base.clone() } else { self.page.url.clone() };
        let Some(sub) = self.page.form.submit(&self.page.dom, form_el, submitter, &resolve_against) else { return Ok(()) };
        let post = matches!(sub.method, forms::Method::Post);
        // pressing Enter twice must not send a POST twice (a duplicate order, say)
        if post && self.nav.is_loading() && self.inflight_post.is_some() {
            return Ok(());
        }
        let from = self.page.url.clone();
        let target = match resolve_link(&from, sub.url.as_str()) {
            Link::Navigate(url) => url,
            Link::Nothing | Link::Fragment(_) => from.clone(), // submitting to the page you're already on
            Link::External(url) => {
                eprintln!("not supported (yet): {url}");
                return Ok(());
            }
        };
        form_target_allowed(&from, &target, post)?;
        self.next_initiator = Some(from.clone());
        if post {
            self.next_post = Some(sub.body);
        }
        // (landing on the page you left is a replace, not a new entry, so submitting a form to
        // its own address needs no special case; and it is a navigation the user asked for)
        self.open(target);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{channel, Receiver};
    use std::sync::Mutex;
    use std::time::Duration;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn link_resolution() {
        let page = u("https://example.com/dir/page.html?q=1");
        assert_eq!(resolve_link(&page, "other.html"), Link::Navigate(u("https://example.com/dir/other.html")));
        assert_eq!(resolve_link(&page, "../up.html#frag"), Link::Navigate(u("https://example.com/up.html#frag")));
        assert_eq!(resolve_link(&page, "/root"), Link::Navigate(u("https://example.com/root")));
        assert_eq!(resolve_link(&page, "//cdn.test/x"), Link::Navigate(u("https://cdn.test/x")));
        assert_eq!(resolve_link(&page, "?q=2"), Link::Navigate(u("https://example.com/dir/page.html?q=2")));
        assert_eq!(resolve_link(&page, "http://other.test/"), Link::Navigate(u("http://other.test/")));
        // same document
        for nothing in ["", "  "] {
            assert_eq!(resolve_link(&page, nothing), Link::Nothing, "{nothing:?}");
        }
        assert_eq!(resolve_link(&page, "#section"), Link::Fragment("section".into()));
        assert_eq!(resolve_link(&page, "#"), Link::Fragment(String::new()));
        assert_eq!(resolve_link(&page, "page.html?q=1#other"), Link::Fragment("other".into()));
        assert_eq!(resolve_link(&page, "https://example.com/dir/page.html?q=1#x%20y"), Link::Fragment("x%20y".into()));
        // a different query is a different document, fragment or not
        assert_eq!(resolve_link(&page, "page.html?q=2#other"), Link::Navigate(u("https://example.com/dir/page.html?q=2#other")));
        // schemes we don't handle, and web -> local file are refused
        for external in ["mailto:a@b.c", "javascript:void(0)", "tel:123", "data:text/html,hi", "file:///etc/passwd", "ftp://x.test/y"] {
            assert!(matches!(resolve_link(&page, external), Link::External(_)), "{external}");
        }
        // control characters hidden inside a scheme can't smuggle a javascript: link through
        assert!(matches!(resolve_link(&page, "java\nscript:alert(1)"), Link::External(_)));
        assert!(matches!(resolve_link(&page, "  JaVaScRiPt:alert(1)"), Link::External(_)));
        // local pages can link to local files, with percent-encoding handled by the URL parser
        let local = u("file:///site/pages/index.html");
        assert_eq!(resolve_link(&local, "a%20b.html?x=1#top"), Link::Navigate(u("file:///site/pages/a%20b.html?x=1#top")));
        assert_eq!(resolve_link(&local, "file:///other.html"), Link::Navigate(u("file:///other.html")));
    }

    #[test]
    fn links_resolve_against_the_base_href_not_the_page_address() {
        let doc = u("https://example.com/a/page.html");
        let base = u("https://cdn.example.com/other/dir/");
        assert_eq!(resolve_link_in(&doc, &base, "rel.html"), Link::Navigate(u("https://cdn.example.com/other/dir/rel.html")));
        assert_eq!(resolve_link_in(&doc, &base, "/abs"), Link::Navigate(u("https://cdn.example.com/abs")));
        // with a base elsewhere, `#x` is a link to the BASE's document, which is another page
        assert_eq!(resolve_link_in(&doc, &base, "#x"), Link::Navigate(u("https://cdn.example.com/other/dir/#x")));
        // a link spelled out in full to this very page still only scrolls
        assert_eq!(resolve_link_in(&doc, &base, "https://example.com/a/page.html#sec"), Link::Fragment("sec".into()));
        // a base that points at a local file doesn't let a web page open it
        let sneaky = u("file:///etc/");
        assert!(matches!(resolve_link_in(&doc, &sneaky, "passwd"), Link::External(_)));
        // without a base href the two forms agree
        assert_eq!(resolve_link_in(&doc, &doc, "x.html"), resolve_link(&doc, "x.html"));
    }

    #[test]
    fn a_tab_follows_links_relative_to_the_base_href() {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.page = Page::from_source(&u("file:///tmp/dir/p.html"), "<base href='file:///elsewhere/'><a href=x.html>x</a>");
        assert_eq!(tab.link_target("x.html"), Some(u("file:///elsewhere/x.html")));
        assert_eq!(tab.link_target("#top"), Some(u("file:///elsewhere/#top")));
        let mut plain = Tab::blank(2, queue().0);
        plain.page = Page::from_source(&u("file:///tmp/dir/p.html"), "<a href=x.html>x</a>");
        assert_eq!(plain.link_target("x.html"), Some(u("file:///tmp/dir/x.html")));
        assert_eq!(plain.link_target("#top"), None, "a fragment of the page itself is not a navigation");
    }

    #[test]
    fn a_form_action_resolves_against_the_base_href() {
        let fonts = crate::fonts::Fonts::load();
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.page = Page::from_source(&u("file:///tmp/dir/p.html"), "<base href='file:///elsewhere/'><form action=go><input name=q value=1></form>");
        tab.page.relayout(&fonts, 400.0);
        let input = tab.page.layout.controls[0].node;
        assert_eq!(tab.submit_form(input, None), Ok(()));
        assert_eq!(tab.nav.pending().unwrap().url, u("file:///elsewhere/go?q=1"));
        // a button's own formaction does too
        let mut tab = Tab::blank(2, queue().0);
        tab.page = Page::from_source(
            &u("file:///tmp/dir/p.html"),
            "<base href='file:///elsewhere/'><form><input name=q value=1><button formaction=other>go</button></form>",
        );
        tab.page.relayout(&fonts, 400.0);
        let (input, button) = (tab.page.layout.controls[0].node, tab.page.layout.controls[1].node);
        assert_eq!(tab.submit_form(input, Some(button)), Ok(()));
        assert_eq!(tab.nav.pending().unwrap().url, u("file:///elsewhere/other?q=1"));
        // but a form with no action submits to the page itself, whatever the base says
        let mut tab = Tab::blank(3, queue().0);
        tab.page = Page::from_source(&u("file:///tmp/dir/p.html"), "<base href='file:///elsewhere/'><form><input name=q value=1></form>");
        tab.page.relayout(&fonts, 400.0);
        let input = tab.page.layout.controls[0].node;
        assert_eq!(tab.submit_form(input, None), Ok(()));
        assert_eq!(tab.nav.pending().unwrap().url, u("file:///tmp/dir/p.html?q=1"));
    }

    #[test]
    fn form_submissions_go_through_the_same_link_safety_checks() {
        // a form on a web page can't send you to a local file or a javascript: URL
        let page = u("https://example.com/form.html");
        for action in ["file:///etc/passwd", "javascript:alert(1)", "mailto:a@b.c"] {
            assert!(matches!(resolve_link(&page, action), Link::External(_)), "{action}");
        }
        // a link or GET form to the very same URL loads it again (landing on the page you
        // left replaces its history entry instead of adding one), but with a fragment it only scrolls
        assert_eq!(resolve_link(&page, "https://example.com/form.html"), Link::Navigate(u("https://example.com/form.html")));
        assert_eq!(resolve_link(&page, "form.html#x"), Link::Fragment("x".into()));
        assert_eq!(resolve_link(&page, "https://example.com/form.html?q=rust"), Link::Navigate(u("https://example.com/form.html?q=rust")));
    }

    #[test]
    fn forms_never_downgrade_to_plain_http_or_post_to_non_http() {
        // an https login page whose form action is http:// sent the password in the clear
        let secure = u("https://bank.test/login");
        assert!(form_target_allowed(&secure, &u("https://bank.test/auth"), true).is_ok());
        for post in [true, false] {
            assert!(form_target_allowed(&secure, &u("http://bank.test/auth"), post).is_err());
            assert!(form_target_allowed(&secure, &u("http://other.test/"), post).is_err());
            // a server on this machine isn't "the network"
            assert!(form_target_allowed(&secure, &u("http://localhost:8080/x"), post).is_ok());
            assert!(form_target_allowed(&secure, &u("http://127.0.0.1/x"), post).is_ok());
        }
        // plain-http pages may submit anywhere http(s)
        let plain = u("http://old.test/form");
        assert!(form_target_allowed(&plain, &u("http://old.test/go"), true).is_ok());
        assert!(form_target_allowed(&plain, &u("https://new.test/go"), true).is_ok());
        // a POST to a file: URL is an error up front, not an error page in your history
        let local = u("file:///site/form.html");
        assert!(form_target_allowed(&local, &u("file:///site/x.html"), true).is_err());
        assert!(form_target_allowed(&local, &u("file:///site/x.html"), false).is_ok(), "GET to a file is a normal navigation");
    }

    #[test]
    fn ipv6_and_trailing_dot_loopback_targets_are_local_too() {
        // `host_str()` keeps the brackets of an IPv6 address, so [::1] used to count as remote
        let secure = u("https://app.test/");
        for local in ["http://[::1]:8000/x", "http://localhost./x", "http://LOCALHOST/x", "http://[0:0:0:0:0:0:0:1]/x"] {
            assert!(form_target_allowed(&secure, &u(local), true).is_ok(), "{local}");
        }
        for remote in ["http://[2001:db8::1]/x", "http://localhost.evil.test./x", "http://128.0.0.1/x"] {
            assert!(form_target_allowed(&secure, &u(remote), true).is_err(), "{remote}");
        }
    }

    #[test]
    fn look_alike_hosts_are_not_loopback() {
        assert!(is_loopback_host("localhost") && is_loopback_host("127.0.0.1") && is_loopback_host("127.1.2.3"));
        assert!(is_loopback_host("[::1]") && is_loopback_host("::1") && is_loopback_host("localhost."));
        assert!(!is_loopback_host("localhost.evil.com") && !is_loopback_host("127.evil.com") && !is_loopback_host("128.0.0.1"));
        assert!(!is_loopback_host("") && !is_loopback_host("[]") && !is_loopback_host("."));
    }

    // ---------- navigation tickets ----------

    #[test]
    fn only_the_newest_navigation_may_complete() {
        let mut nav = Navigation::default();
        assert!(!nav.is_loading());
        let first = nav.start(u("https://home.test/"), u("https://a.test/"), Hist::Push, None);
        assert!(nav.is_loading());
        // the user clicks something else before the first load finishes
        let second = nav.start(u("https://home.test/"), u("https://b.test/"), Hist::Push, Some(42.0));
        assert_ne!(first, second);
        // the slow first load arrives late: it must be ignored, and not disturb the second
        assert_eq!(nav.finish(first), None);
        assert!(nav.is_loading(), "the second navigation is still pending");
        let done = nav.finish(second).expect("the newest load completes");
        assert_eq!((done.hist, done.url, done.keep_scroll), (Hist::Push, u("https://b.test/"), Some(42.0)));
        assert!(!nav.is_loading());
        // a duplicate delivery of the same ticket completes nothing
        assert_eq!(nav.finish(second), None);
    }

    #[test]
    fn cancelling_invalidates_the_load_in_flight() {
        let mut nav = Navigation::default();
        assert!(!nav.cancel(), "nothing to cancel");
        let t = nav.start(u("https://home.test/"), u("https://a.test/"), Hist::Replace, None);
        assert!(nav.cancel());
        assert!(!nav.is_loading());
        assert_eq!(nav.finish(t), None, "a cancelled load that still arrives is dropped");
        // and a new navigation after a cancel works normally
        let t2 = nav.start(u("https://home.test/"), u("https://c.test/"), Hist::Push, None);
        assert!(nav.finish(t2).is_some());
    }

    // ---------- history ----------

    /// The user is on C, having come from A then B.
    fn on_c() -> (History, Url) {
        let h = History { back: vec![u("https://a.test/"), u("https://b.test/")], forward: vec![] };
        (h, u("https://c.test/"))
    }

    #[test]
    fn a_cancelled_back_navigation_leaves_history_untouched() {
        // Back used to pop the stack and push the forward stack the moment it was pressed,
        // so cancelling lost B and left Forward pointing at the page you were already on.
        let (history, c) = on_c();
        let mut nav = Navigation::default();
        let before = History { back: history.back.clone(), forward: history.forward.clone() };
        let target = history.back_target().cloned().unwrap();
        assert_eq!(target, u("https://b.test/"));
        let t = nav.start(c, target, Hist::Back, None);
        assert!(nav.cancel());
        assert_eq!(nav.finish(t), None); // nothing lands, so nothing is applied
        assert_eq!(history, before);
    }

    #[test]
    fn back_then_link_click_keeps_the_pages_you_came_from() {
        // back (starts), then click a link before it lands: the link wins, and B must survive
        let (mut history, c) = on_c();
        let mut nav = Navigation::default();
        let _back = nav.start(c.clone(), u("https://b.test/"), Hist::Back, None);
        let link = nav.start(c.clone(), u("https://d.test/"), Hist::Push, None);
        let done = nav.finish(link).unwrap();
        history.apply(done.hist, done.from, &u("https://d.test/"));
        assert_eq!(history.back, [u("https://a.test/"), u("https://b.test/"), c]);
        assert!(history.forward.is_empty());
    }

    #[test]
    fn back_and_forward_move_one_step_when_they_land() {
        let (mut history, c) = on_c();
        history.apply(Hist::Back, c.clone(), &u("https://b.test/")); // now on B
        assert_eq!((history.back.clone(), history.forward.clone()), (vec![u("https://a.test/")], vec![c.clone()]));
        history.apply(Hist::Back, u("https://b.test/"), &u("https://a.test/")); // now on A
        assert_eq!((history.back.len(), history.forward.len()), (0, 2));
        assert_eq!(history.back_target(), None);
        history.apply(Hist::Forward, u("https://a.test/"), &u("https://b.test/")); // forward to B
        assert_eq!(history.back, [u("https://a.test/")]);
        assert_eq!(history.forward_target(), Some(&c));
        // a fresh navigation drops the forward stack
        history.apply(Hist::Push, u("https://b.test/"), &u("https://z.test/"));
        assert!(history.forward.is_empty());
        assert_eq!(history.back, [u("https://a.test/"), u("https://b.test/")]);
    }

    #[test]
    fn landing_on_the_page_you_left_is_a_replace_not_a_new_entry() {
        // typing X when the page is X/final (X redirects there) used to push a duplicate each time
        let mut history = History { back: vec![u("https://a.test/")], forward: vec![u("https://f.test/")] };
        let here = u("https://x.test/final");
        for _ in 0..3 {
            history.apply(Hist::Push, here.clone(), &here);
        }
        assert_eq!(history.back, [u("https://a.test/")], "no duplicates");
        assert_eq!(history.forward, [u("https://f.test/")], "forward survives a replace");
        // and reload never changes history
        history.apply(Hist::Replace, here.clone(), &here);
        assert_eq!(history.back.len(), 1);
    }

    #[test]
    fn reload_during_a_navigation_restarts_that_navigation() {
        // the load being restarted is the pending one, with its original history behaviour
        let mut nav = Navigation::default();
        nav.start(u("https://home.test/"), u("https://b.test/"), Hist::Push, None);
        let p = nav.pending().unwrap();
        let (url, hist) = (p.url.clone(), p.hist);
        assert_eq!((url.as_str(), hist), ("https://b.test/", Hist::Push));
        nav.start(u("https://home.test/"), url, hist, None);
        assert_eq!(nav.pending().unwrap().from, u("https://home.test/"), "still replacing the page on screen");
    }

    // ---------- whole tabs, loading real (local) pages ----------

    /// A notifier that queues loader results for the test to deliver by hand.
    fn queue() -> (Notify, Receiver<UserEvent>) {
        let (tx, rx) = channel();
        let tx = Mutex::new(tx);
        (Arc::new(move |e| drop(tx.lock().unwrap().send(e))), rx)
    }

    fn site(name: &str) -> (std::path::PathBuf, Url, Url) {
        let dir = std::env::temp_dir().join(format!("mybrowser-tab-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.html"), "<title>Page A</title><a href=b.html>to b</a><form action=b.html><input name=q value=1></form>").unwrap();
        std::fs::write(dir.join("b.html"), "<title>Page B</title><p>second</p>").unwrap();
        (dir.clone(), Url::from_file_path(dir.join("a.html")).unwrap(), Url::from_file_path(dir.join("b.html")).unwrap())
    }

    /// Wait for the next loader result and hand it to its tab.
    fn deliver(tab: &mut Tab, rx: &Receiver<UserEvent>) -> bool {
        let UserEvent::Loaded { tab: id, ticket, page } = rx.recv_timeout(Duration::from_secs(20)).expect("the loader reports back");
        assert_eq!(id, tab.id, "events carry their tab's id");
        tab.finish_load(ticket, page.0)
    }

    #[test]
    fn a_tab_loads_navigates_and_keeps_its_own_history() {
        let (_dir, a, b) = site("history");
        let (notify, rx) = queue();
        let mut tab = Tab::new(7, a.clone(), None, notify);
        assert!(tab.is_loading());
        assert_eq!(tab.address(), a.to_string(), "the bar shows where we are going while loading");
        assert!(deliver(&mut tab, &rx));
        assert_eq!((tab.title().as_str(), tab.address()), ("Page A", a.to_string()));
        assert!(tab.history.back.is_empty());
        // follow a link: history only changes when it lands
        assert!(tab.follow("b.html"));
        assert!(tab.history.back.is_empty(), "not yet");
        assert!(deliver(&mut tab, &rx));
        assert_eq!((tab.title().as_str(), tab.history.back.clone()), ("Page B", vec![a.clone()]));
        // back, then forward
        tab.go_back();
        assert!(!tab.is_loading(), "Back shows the page we still hold, without a load");
        assert_eq!((tab.title().as_str(), tab.history.forward.clone()), ("Page A", vec![b.clone()]));
        tab.go_forward();
        assert!(!tab.is_loading());
        assert_eq!(tab.title(), "Page B");
        // reload keeps history and your scroll position
        tab.scroll = 33.0;
        tab.reload();
        assert!(deliver(&mut tab, &rx));
        assert_eq!((tab.scroll, tab.history.back.len()), (33.0, 1));
    }

    #[test]
    fn a_late_or_cancelled_load_never_replaces_the_page() {
        let (_dir, a, b) = site("late");
        let (notify, rx) = queue();
        let mut tab = Tab::new(1, a.clone(), None, notify);
        assert!(deliver(&mut tab, &rx));
        // start loading B, change your mind, and let the result arrive anyway
        tab.open(b.clone());
        assert!(tab.cancel_load());
        assert!(!tab.is_loading());
        assert!(!deliver(&mut tab, &rx), "the cancelled result is dropped");
        assert_eq!((tab.title().as_str(), tab.history.back.len()), ("Page A", 0));
        assert!(!tab.cancel_load(), "nothing left to cancel");
    }

    #[test]
    fn cancelling_the_first_load_of_a_new_tab_leaves_a_blank_tab() {
        // the "Loading…" stand-in page used to stay forever, titled with the host name
        let (_dir, a, _b) = site("cancel-first");
        let (notify, rx) = queue();
        let mut tab = Tab::new(1, a.clone(), Some(u("https://opener.test/")), notify);
        assert!(tab.is_loading());
        assert!(tab.cancel_load());
        assert_eq!((tab.title().as_str(), tab.address().as_str()), ("New Tab", ""));
        assert!(tab.page.dom.text_content(0).contains("New tab"), "{}", tab.page.dom.text_content(0));
        assert!(!deliver(&mut tab, &rx), "the abandoned load is dropped");
        assert!(tab.history.back.is_empty());
        // a tab that has shown a real page keeps it when a later load is cancelled
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        tab.open(a.clone());
        assert!(tab.cancel_load());
        assert_eq!(tab.title(), "Page A");
    }

    #[test]
    fn tabs_are_independent_of_each_other() {
        let (_dir, a, b) = site("independent");
        let (notify, rx) = queue();
        let mut first = Tab::new(1, a.clone(), None, notify.clone());
        let mut second = Tab::new(2, b.clone(), None, notify);
        // two results arrive in either order; each goes to the tab that asked
        for _ in 0..2 {
            let UserEvent::Loaded { tab, ticket, page } = rx.recv_timeout(Duration::from_secs(20)).unwrap();
            let target = if tab == 1 { &mut first } else { &mut second };
            assert!(target.finish_load(ticket, page.0));
        }
        assert_eq!((first.title().as_str(), second.title().as_str()), ("Page A", "Page B"));
        first.follow("b.html");
        assert!(first.is_loading() && !second.is_loading());
        let UserEvent::Loaded { tab, ticket, page } = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        assert_eq!(tab, 1);
        assert!(first.finish_load(ticket, page.0));
        assert_eq!((first.history.back.len(), second.history.back.len()), (1, 0), "only the first tab moved");
    }

    #[test]
    fn a_blank_tab_has_an_empty_address_and_a_new_tab_title() {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(3, notify);
        assert_eq!((tab.address().as_str(), tab.title().as_str()), ("", "New Tab"));
        assert!(!tab.is_loading());
        tab.reload(); // nothing to reload
        assert!(!tab.is_loading());
        tab.go_back();
        tab.go_forward();
        assert!(!tab.is_loading());
        assert!(!tab.follow("x.html"), "links on the blank page go nowhere");
    }

    #[test]
    fn navigating_away_from_and_back_to_a_blank_tab_works() {
        let (_dir, a, _b) = site("blank");
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.history.back, [blank_url()], "the blank page is in history");
        tab.go_back();
        assert!(deliver(&mut tab, &rx), "about:blank loads without touching the network");
        assert_eq!((tab.title().as_str(), tab.address().as_str()), ("New Tab", ""));
    }

    #[test]
    fn submitting_a_form_navigates_with_its_data() {
        let (_dir, a, b) = site("form");
        let (notify, rx) = queue();
        let mut tab = Tab::new(1, a, None, notify);
        assert!(deliver(&mut tab, &rx));
        let fonts = crate::fonts::Fonts::load();
        tab.page.relayout(&fonts, 400.0);
        let input = tab.page.layout.controls[0].node;
        assert_eq!(tab.submit_form(input, None), Ok(()));
        assert!(deliver(&mut tab, &rx));
        assert_eq!((tab.title().as_str(), tab.page.url.path()), ("Page B", b.path()));
        assert_eq!(tab.page.url.query(), Some("q=1"));
        // a refused submission reports why and sends nothing
        let (notify, _rx) = queue();
        let mut secure = Tab::blank(2, notify);
        secure.page = Page::from_source(&u("https://bank.test/login"), "<form method=post action='http://evil.test/x'><input name=pw value=s><button>go</button></form>");
        secure.page.relayout(&fonts, 400.0);
        let pw = secure.page.layout.controls[0].node;
        let err = secure.submit_form(pw, None).unwrap_err();
        assert!(err.contains("plain http"), "{err}");
        assert!(!secure.is_loading(), "nothing was sent");
    }

    // ---------- in-page anchors ----------

    /// A tab showing a tall page with sections, laid out at 400px.
    fn tall_tab() -> Tab {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        let src = "<style>body{margin:0}p{margin:0;height:500px}</style>\
                   <a id=l1 href='#two'>to two</a><a id=l2 href='#nope'>nope</a><a id=l3 href='#'>top</a>\
                   <p>one</p><h2 id=two style='margin:0'>Two</h2><p>x</p><div id='sp ace'></div><p>y</p>";
        tab.page = Page::from_source(&u("file:///tmp/long.html"), src);
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        tab.laid_out_width = 400.0;
        tab
    }

    #[test]
    fn clicking_a_fragment_link_scrolls_to_its_target_without_navigating() {
        let mut tab = tall_tab();
        let two = tab.page.layout.anchor_y("two").expect("the heading is a target");
        assert!(two > 400.0, "far down the page: {two}");
        assert!(!tab.follow("#two"), "no navigation starts");
        assert!(!tab.is_loading() && tab.history.back.len() == 1, "the fragment is a history entry");
        assert_eq!(tab.page.url.fragment(), Some("two"));
        assert_eq!(tab.scroll, two);
        // a missing target leaves the page where it is, `#` and `#top` go home
        assert!(!tab.follow("#nope"));
        assert_eq!(tab.scroll, two);
        assert!(!tab.follow("#top") && tab.scroll == 0.0);
        tab.scroll = 99.0;
        tab.follow("#");
        assert_eq!(tab.scroll, 0.0);
        // percent-encoded fragments find ids with spaces, and a full URL to this page works too
        assert!(!tab.follow("#sp%20ace"));
        assert!(tab.scroll > two);
        tab.follow("long.html#two");
        assert_eq!(tab.scroll, two);
        assert!(!tab.is_loading());
    }

    #[test]
    fn fragment_links_add_history_entries_that_back_and_forward_walk_without_loading() {
        let mut tab = tall_tab();
        let base = tab.page.url.clone();
        let two = tab.page.layout.anchor_y("two").unwrap();
        tab.follow("#two");
        tab.follow("#two"); // the same place again adds nothing
        assert_eq!(tab.history.back, vec![base.clone()]);
        tab.follow("#");
        assert_eq!(tab.history.back.len(), 2);
        assert_eq!(tab.address(), format!("{base}#"));
        tab.go_back();
        assert!(!tab.is_loading(), "same document: no load");
        assert_eq!((tab.page.url.fragment(), tab.scroll), (Some("two"), two));
        tab.go_back();
        assert_eq!((tab.page.url.clone(), tab.scroll), (base.clone(), 0.0));
        assert!(!tab.is_loading() && tab.history.back.is_empty() && tab.history.forward.len() == 2);
        tab.go_forward();
        assert_eq!((tab.page.url.fragment(), tab.scroll), (Some("two"), two));
        // a new fragment visit drops the forward entries
        tab.follow("#sp%20ace");
        assert!(tab.history.forward.is_empty());
        // reloading keeps the place
        tab.scroll = 123.0;
        tab.reload();
        assert_eq!(tab.scroll, 123.0, "reload keeps scroll (applied on landing)");
    }

    #[test]
    fn back_and_forward_return_to_where_each_page_was_scrolled() {
        let (dir, a, b) = site("scrollmem");
        std::fs::write(dir.join("long.html"), "<style>body{margin:0}p{margin:0;height:900px}</style><p>one</p><p>two</p>").unwrap();
        let long = Url::from_file_path(dir.join("long.html")).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(long.clone());
        assert!(deliver(&mut tab, &rx));
        tab.scroll = 700.0;
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.scroll, 0.0, "a new page starts at the top");
        tab.scroll = 40.0;
        tab.go_back();
        assert_eq!((tab.page.url.clone(), tab.scroll), (long.clone(), 700.0), "Back restores the position");
        tab.go_forward();
        assert_eq!((tab.page.url.clone(), tab.scroll), (b.clone(), 40.0), "and so does Forward");
        // a page never visited before has nothing to restore
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.scroll, 0.0);
        // fragment moves on one page remember their own positions too
        tab.open(long.clone());
        assert!(deliver(&mut tab, &rx));
        tab.scroll = 300.0;
        tab.follow("#nowhere");
        tab.scroll = 650.0;
        tab.go_back();
        assert_eq!(tab.scroll, 300.0);
        tab.go_forward();
        assert_eq!(tab.scroll, 650.0);
    }

    /// A throwaway HTTP server: (path, content type, status, body). Answers any number of requests.
    fn serve_forever(routes: Vec<(&'static str, &'static str, u16, Vec<u8>)>) -> u16 {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes = std::sync::Arc::new(routes);
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { continue };
                let routes = routes.clone();
                std::thread::spawn(move || {
                    let (mut seen, mut buf) = (vec![], [0u8; 2048]);
                    while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf) {
                            Ok(k) if k > 0 => seen.extend_from_slice(&buf[..k]),
                            _ => return,
                        }
                    }
                    // read a POST body too: closing with unread data can reset the connection and lose the reply
                    let head_end = seen.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let declared = String::from_utf8_lossy(&seen[..head_end])
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok()))
                        .unwrap_or(0);
                    while seen.len() < head_end + declared {
                        match sock.read(&mut buf) {
                            Ok(k) if k > 0 => seen.extend_from_slice(&buf[..k]),
                            _ => break,
                        }
                    }
                    let line = String::from_utf8_lossy(&seen).lines().next().unwrap_or("").to_string();
                    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (ctype, status, body) = match routes.iter().find(|r| r.0 == path) {
                        Some(r) => (r.1, r.2, r.3.clone()),
                        None => ("text/plain", 404, b"nope".to_vec()),
                    };
                    let head = format!("HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(&body);
                });
            }
        });
        port
    }

    fn files_in(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).map(|d| d.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    #[test]
    fn downloads_are_saved_only_when_the_user_asked_and_never_twice() {
        let dir = std::env::temp_dir().join(format!("mybrowser-tabdl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // (the only test that sets this; nothing else in the suite saves files)
        unsafe { std::env::set_var("MYBROWSER_DOWNLOADS", &dir) };
        let payload = vec![0u8, 159, 146, 150, 7];
        let port = serve_forever(vec![
            ("/home.html", "text/html", 200, b"<title>Home</title><p>home</p>".to_vec()),
            ("/f.zip", "application/zip", 200, payload.clone()),
            ("/lure.html", "text/html", 200, b"<meta http-equiv=refresh content='0;url=/payload.bin'><p>lure</p>".to_vec()),
            ("/payload.bin", "application/octet-stream", 200, payload.clone()),
            ("/missing.zip", "application/zip", 404, payload.clone()),
            ("/ping", "application/octet-stream", 204, vec![]),
            ("/slow.zip", "application/zip", 200, payload.clone()),
            ("/partial.bin", "application/octet-stream", 206, payload.clone()),
            ("/postdl", "application/octet-stream", 200, payload.clone()),
            ("/f2.zip", "application/zip", 200, payload.clone()),
            ("/f2.zip?x=1", "application/zip", 200, payload.clone()),
            ("/other.html", "text/html", 200, b"<p>other</p>".to_vec()),
        ]);
        let url = |p: &str| Url::parse(&format!("http://127.0.0.1:{port}{p}")).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(url("/home.html"));
        assert!(deliver(&mut tab, &rx));
        let back_before = tab.history.back.clone();

        // a link or typed address starts a download: the file is saved and the page stays put
        tab.open(url("/f.zip"));
        assert!(deliver(&mut tab, &rx));
        assert_eq!(files_in(&dir), ["f.zip"]);
        assert_eq!(std::fs::read(dir.join("f.zip")).unwrap(), payload);
        assert!(tab.take_notice().unwrap().starts_with("Saved f.zip"));
        assert_eq!(tab.take_notice(), None, "the note is given once");
        assert_eq!(tab.page.url, url("/home.html"), "you stay on the page you were on");
        assert_eq!(tab.history.back, back_before, "a download is not a history entry");

        // reload, Back and Forward act on that page and can't fetch the file again
        tab.reload();
        assert!(deliver(&mut tab, &rx));
        tab.go_back();
        if tab.is_loading() {
            assert!(deliver(&mut tab, &rx));
        }
        assert_eq!(files_in(&dir), ["f.zip"]);

        // a cancelled load, and one that was superseded, write nothing
        tab.open(url("/slow.zip"));
        tab.cancel_load();
        assert!(!deliver(&mut tab, &rx), "a cancelled load is dropped");
        tab.open(url("/slow.zip"));
        tab.open(url("/home.html"));
        let landed = [deliver(&mut tab, &rx), deliver(&mut tab, &rx)]; // (either may report first)
        assert_eq!(landed.iter().filter(|l| **l).count(), 1, "only the newest load lands: {landed:?}");
        assert_eq!(tab.page.url, url("/home.html"));
        assert_eq!(files_in(&dir), ["f.zip"]);

        // a page that sends you to a file without being asked (meta refresh) saves nothing
        tab.open(url("/lure.html"));
        assert!(deliver(&mut tab, &rx));
        assert!(tab.is_loading(), "the refresh started");
        assert!(deliver(&mut tab, &rx));
        assert_eq!(files_in(&dir), ["f.zip"], "no drive-by download");
        assert!(tab.take_notice().unwrap().starts_with("Not saved"));

        // an error response and an empty one are not downloads
        for path in ["/missing.zip", "/ping"] {
            tab.open(url(path));
            assert!(deliver(&mut tab, &rx));
            assert_eq!(files_in(&dir), ["f.zip"], "{path}");
            assert!(tab.take_notice().is_none(), "{path}");
        }

        // a piece of a file (206) is not a whole file
        tab.open(url("/partial.bin"));
        assert!(deliver(&mut tab, &rx));
        assert_eq!(files_in(&dir), ["f.zip"]);

        // a form the user submits may download, even when it posts to its own address
        tab.page = Page::from_source(&url("/postdl"), "<form method=post><input name=a value=1><input type=submit></form>");
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        let submit = tab.page.layout.controls.last().unwrap().node;
        tab.activate_control(submit).unwrap();
        assert!(deliver(&mut tab, &rx));
        let note = tab.take_notice();
        assert_eq!(files_in(&dir), ["f.zip", "postdl"], "note: {note:?}, page: {} {:?}", tab.page.url, tab.page.title);
        assert!(note.unwrap().starts_with("Saved postdl"));

        // a GET form submitted to its own address may download too
        tab.page = Page::from_source(&url("/f2.zip?x=1"), "<form method=get action='/f2.zip'><input name=x value=1><input type=submit></form>");
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        let get_submit = tab.page.layout.controls.last().unwrap().node;
        tab.activate_control(get_submit).unwrap();
        assert!(deliver(&mut tab, &rx));
        assert!(files_in(&dir).contains(&"f2.zip".to_string()), "{:?} {:?}", files_in(&dir), tab.take_notice());
        let _ = std::fs::remove_file(dir.join("f2.zip")); // (the next step expects to create it)

        // a note about one download is not shown with the next page
        tab.open(url("/f2.zip"));
        assert!(deliver(&mut tab, &rx));
        tab.open(url("/other.html"));
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.take_notice(), None, "the old note is dropped when a new load starts");
        assert_eq!(files_in(&dir), ["f.zip", "f2.zip", "postdl"]);

        // a tab that was opened straight onto a file shows the note page (and saves it)
        let (notify2, rx2) = queue();
        let mut fresh = Tab::new(2, url("/payload.bin"), None, notify2);
        assert!(deliver(&mut fresh, &rx2));
        assert_eq!(files_in(&dir), ["f.zip", "f2.zip", "payload.bin", "postdl"]);
        assert!(fresh.page.title.starts_with("Download payload.bin"), "{}", fresh.page.title);
        // ...and the note page doesn't become a Back entry once the tab moves on
        fresh.open(url("/home.html"));
        assert!(deliver(&mut fresh, &rx2));
        assert!(!fresh.history.back.contains(&url("/payload.bin")), "{:?}", fresh.history.back);
        // a tab brought back from the closed list saves nothing, so closing and reopening can't copy a file
        let (notify3, rx3) = queue();
        let mut reopened = Tab::new(3, url("/payload.bin"), None, notify3);
        reopened.mark_restored();
        assert!(deliver(&mut reopened, &rx3));
        assert_eq!(files_in(&dir), ["f.zip", "f2.zip", "payload.bin", "postdl"], "no second copy");
        assert!(reopened.take_notice().unwrap().starts_with("Not saved"));
        unsafe { std::env::remove_var("MYBROWSER_DOWNLOADS") };
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_delay_meta_refresh_sends_the_tab_on_and_replaces_its_history_entry() {
        let (dir, a, b) = site("metarefresh");
        std::fs::write(dir.join("hop.html"), "<meta http-equiv=refresh content='0; url=b.html'><p>moved</p>").unwrap();
        std::fs::write(dir.join("slow.html"), "<meta http-equiv=refresh content='5; url=b.html'><p>wait</p>").unwrap();
        std::fs::write(dir.join("loop1.html"), "<meta http-equiv=REFRESH content=\"0;URL='loop2.html'\">").unwrap();
        std::fs::write(dir.join("loop2.html"), "<meta http-equiv=refresh content='0;loop1.html'>").unwrap();
        std::fs::write(dir.join("self.html"), "<meta http-equiv=refresh content='0; url=self.html'><p>me</p>").unwrap();
        std::fs::write(dir.join("evil.html"), "<meta http-equiv=refresh content='0; url=javascript:alert(1)'>").unwrap();
        let file = |n: &str| Url::from_file_path(dir.join(n)).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        tab.open(file("hop.html"));
        assert!(deliver(&mut tab, &rx));
        assert!(tab.is_loading(), "the refresh started a load");
        assert_eq!(tab.inflight_initiator, Some(file("hop.html")), "the refreshing page is the initiator (SameSite)");
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.page.url, b);
        assert_eq!(tab.history.back.last(), Some(&a), "Back goes to where the refresh started from");
        assert!(!tab.history.back.contains(&file("hop.html")), "the refreshing page isn't left in the history");
        // a delay is not honoured (there are no timers), and neither are self-refreshes or odd schemes
        for name in ["slow.html", "self.html", "evil.html"] {
            tab.open(file(name));
            assert!(deliver(&mut tab, &rx));
            assert!(!tab.is_loading(), "{name} stays put");
            assert_eq!(tab.page.url, file(name));
        }
        // two pages refreshing to each other are cut off after a few hops
        tab.open(file("loop1.html"));
        let mut loads = 0;
        loop {
            assert!(deliver(&mut tab, &rx));
            loads += 1;
            if !tab.is_loading() {
                break;
            }
            assert!(loads < 20, "never ends");
        }
        assert_eq!(loads, 1 + MAX_REFRESH_HOPS as usize);
        // a fresh navigation starts the count again
        tab.open(file("hop.html"));
        assert!(deliver(&mut tab, &rx));
        assert!(tab.is_loading());
    }

    #[test]
    fn back_and_forward_bring_back_the_page_as_it_was_including_what_was_typed() {
        let (dir, a, b) = site("bfcache");
        std::fs::write(dir.join("c.html"), "<title>C</title><p>third</p>").unwrap();
        let c = Url::from_file_path(dir.join("c.html")).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        let input = tab.page.layout.controls[0].node;
        {
            let Page { dom, form, .. } = &mut tab.page;
            form.type_text(dom, input, "typed");
        }
        tab.scroll = 33.0;
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        // Back: no load, the same page, with the text still in its field and its scroll position
        tab.go_back();
        assert!(!tab.is_loading());
        assert_eq!((tab.page.url.clone(), tab.scroll), (a.clone(), 33.0));
        assert_eq!(tab.page.form.value(&tab.page.dom, input), "1typed", "the field keeps what was typed");
        // and Forward brings back the page we left, too
        tab.go_forward();
        assert!(!tab.is_loading());
        assert_eq!(tab.page.url, b);
        // a third page, then back twice: both come from memory
        tab.open(c.clone());
        assert!(deliver(&mut tab, &rx));
        tab.go_back();
        tab.go_back();
        assert!(!tab.is_loading());
        assert_eq!(tab.page.form.value(&tab.page.dom, input), "1typed");
    }

    #[test]
    fn a_no_store_page_is_never_kept_for_back() {
        let (_dir, a, b) = site("nostore");
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        tab.page.no_store = true; // (as if the server had sent Cache-Control: no-store)
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        tab.go_back();
        assert!(tab.is_loading(), "fetched again instead of restored from memory");
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.page.url, a);
        assert!(!tab.page.no_store);
    }

    #[test]
    fn the_cache_review_regressions() {
        let (dir, a, b) = site("bfreview");
        let file = |n: &str| Url::from_file_path(dir.join(n)).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);

        // an error page is not kept: Back tries the address again
        let later = file("later.html");
        tab.open(a.clone());
        assert!(deliver(&mut tab, &rx));
        tab.open(later.clone());
        assert!(deliver(&mut tab, &rx));
        assert!(tab.page.is_error);
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        std::fs::write(dir.join("later.html"), "<title>Now fine</title>").unwrap();
        tab.go_back();
        assert!(tab.is_loading(), "the failed page was not remembered");
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.title(), "Now fine");

        // fragment steps survive a restore from memory
        std::fs::write(dir.join("doc.html"), "<p id=y>y</p>").unwrap();
        let doc = file("doc.html");
        tab.open(doc.clone());
        assert!(deliver(&mut tab, &rx));
        tab.follow("#y");
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        assert!(tab.go_back(), "doc#y comes from memory");
        assert_eq!(tab.page.url.fragment(), Some("y"));
        assert!(!tab.go_back(), "the step to doc itself is a same-document move");
        assert!(!tab.is_loading());
        assert_eq!(tab.page.url, doc);

        // a copy kept from an earlier visit can't come back in place of the page that is shown
        std::fs::write(dir.join("v.html"), "<title>old</title>").unwrap();
        let v = file("v.html");
        tab.open(v.clone());
        assert!(deliver(&mut tab, &rx));
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx)); // v is cached
        tab.open(file("c.html")); // a load in flight...
        std::fs::write(dir.join("v.html"), "<title>new</title>").unwrap();
        tab.go_back(); // ...so Back goes to the network, not the cache
        let _ = (deliver(&mut tab, &rx), deliver(&mut tab, &rx));
        assert_eq!(tab.title(), "new");
        let shown = (tab.page.url.clone(), tab.history.back.len());
        assert!(tab.page_cache.iter().all(|c| c.key != shown), "the shown page is not also in the cache");
    }

    #[test]
    fn the_page_cache_is_bounded_and_forgets_forward_entries() {
        let (dir, a, b) = site("bfcache2");
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        // more pages than the cache holds: the oldest fall out and are loaded again
        let mut urls = vec![a.clone(), b.clone()];
        for i in 0..(MAX_CACHED_PAGES + 3) {
            std::fs::write(dir.join(format!("p{i}.html")), format!("<title>P{i}</title><p>p{i}</p>")).unwrap();
            urls.push(Url::from_file_path(dir.join(format!("p{i}.html"))).unwrap());
        }
        for u in &urls {
            tab.open(u.clone());
            assert!(deliver(&mut tab, &rx));
        }
        assert!(tab.page_cache.len() <= MAX_CACHED_PAGES);
        let mut loaded = 0;
        for _ in 0..urls.len() - 2 {
            tab.go_back();
            if tab.is_loading() {
                loaded += 1;
                assert!(deliver(&mut tab, &rx));
            }
        }
        assert!(loaded >= 3, "the oldest pages had to be fetched again: {loaded}");
        assert_eq!(tab.page.url, urls[1]);
        // a new navigation throws away the pages that were ahead
        tab.open(urls[3].clone());
        assert!(deliver(&mut tab, &rx));
        assert!(tab.history.forward.is_empty());
        assert!(tab.page_cache.iter().all(|c| c.key.1 <= tab.history.back.len()), "nothing cached from the abandoned future");
        // a reload replaces the page rather than caching the old copy as a history entry
        let before = tab.page_cache.len();
        tab.reload();
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.page_cache.len(), before);
    }

    #[test]
    fn back_only_skips_loading_between_fragments_of_the_same_document() {
        let (dir, _a, _b) = site("fragdoc");
        std::fs::write(dir.join("doc.html"), "<style>p{height:900px}</style><p id=x>one</p><p id=y>two</p>").unwrap();
        let doc = Url::from_file_path(dir.join("doc.html")).unwrap();
        let mut with_x = doc.clone();
        with_x.set_fragment(Some("x"));
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        // a page opened at doc#x, then the same address without a fragment (say, a form's result):
        // Back to doc#x is a different document, so it must load, not just move the address
        tab.open(with_x.clone());
        assert!(deliver(&mut tab, &rx));
        tab.open(doc.clone());
        assert!(deliver(&mut tab, &rx));
        assert_eq!(tab.history.back.last(), Some(&with_x));
        tab.go_back();
        // (not a same-document move: the page held for doc#x is swapped back in, a different document)
        assert_eq!(tab.page.url, with_x);
        assert_eq!(tab.frag_back, 0);
        // fragment clicks on one document still walk without loading
        tab.follow("#y");
        tab.go_back();
        assert!(!tab.is_loading());
        assert_eq!(tab.page.url, with_x);
        tab.go_forward();
        assert!(!tab.is_loading());
        assert_eq!(tab.page.url.fragment(), Some("y"));
        // ...and a fresh load of the page ends that run
        tab.open(doc.clone());
        assert!(deliver(&mut tab, &rx));
        tab.go_back();
        assert!(!tab.is_loading());
        assert_eq!(tab.frag_back, 1, "the restored page is the same document instance, so its fragment step is back");
        assert!(!tab.go_back() && !tab.is_loading(), "and walking it needs no load");
    }

    #[test]
    fn a_fragment_link_into_a_closed_details_opens_it_and_scrolls_there() {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.page = Page::from_source(
            &Url::parse("file:///tmp/frag.html").unwrap(),
            "<style>body{margin:0}p{margin:0;height:600px}</style><p>top</p><details><summary>More</summary><p>one</p><h2 id=deep style='margin:0'>deep</h2></details>",
        );
        let fonts = crate::fonts::Fonts::load();
        tab.page.relayout(&fonts, 400.0);
        tab.laid_out_width = 400.0;
        assert_eq!(tab.page.layout.anchor_y("deep"), None);
        tab.follow("#deep");
        assert_eq!(tab.laid_out_width, -1.0, "the page must be laid out again");
        tab.page.relayout(&fonts, 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert!(tab.scroll > 1000.0, "scrolled to the revealed heading: {}", tab.scroll);
        assert_eq!(tab.page.url.fragment(), Some("deep"));
        // a target that doesn't exist still does nothing
        assert!(!tab.page.reveal_fragment("nope"));
        // with a visible duplicate id further down, the first one (in the closed details) still wins
        tab.page = Page::from_source(
            &Url::parse("file:///tmp/dup.html").unwrap(),
            "<style>body{margin:0}p{margin:0;height:600px}</style><details><summary>More</summary><h2 id=x style='margin:0'>first</h2></details><p>pad</p><h2 id=x style='margin:0'>second</h2>",
        );
        tab.page.relayout(&fonts, 400.0);
        tab.laid_out_width = 400.0;
        tab.scroll = 0.0;
        tab.scroll_to_fragment("x");
        assert_eq!(tab.laid_out_width, -1.0, "the closed details is opened first");
        tab.page.relayout(&fonts, 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert!(tab.scroll < 100.0, "scrolled to the first #x, not the second: {}", tab.scroll);
    }

    #[test]
    fn clicking_a_details_heading_opens_and_closes_it() {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.page = Page::from_source(&Url::parse("file:///tmp/d.html").unwrap(), "<details><summary>Head</summary><p>inside</p></details>");
        let fonts = crate::fonts::Fonts::load();
        tab.page.relayout(&fonts, 400.0);
        let has_inside = |t: &Tab| t.page.layout.items.iter().any(|i| matches!(i, crate::layout::Item::Text { text, .. } if text == "inside"));
        assert!(!has_inside(&tab));
        let summary = tab.page.layout.controls[0].node;
        tab.activate_control(summary).unwrap();
        assert!(!tab.is_loading());
        tab.page.relayout(&fonts, 400.0);
        assert!(has_inside(&tab));
        tab.activate_control(summary).unwrap();
        tab.page.relayout(&fonts, 400.0);
        assert!(!has_inside(&tab));
    }

    #[test]
    fn clicking_a_reset_button_clears_what_was_typed() {
        let (notify, _rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.page = Page::from_source(&Url::parse("file:///tmp/r.html").unwrap(), "<form><input name=q value=orig><input type=reset value=Reset></form>");
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        let (input, reset) = (tab.page.layout.controls[0].node, tab.page.layout.controls[1].node);
        tab.activate_control(input).unwrap();
        let Page { dom, form, .. } = &mut tab.page;
        form.type_text(dom, input, " changed");
        assert_eq!(tab.page.form.value(&tab.page.dom, input), "orig changed");
        tab.activate_control(reset).unwrap();
        assert_eq!(tab.page.form.value(&tab.page.dom, input), "orig");
        assert!(!tab.is_loading(), "reset never navigates");
    }

    #[test]
    fn scroll_positions_belong_to_history_entries_not_urls() {
        let (dir, _a, b) = site("scrollentries");
        std::fs::write(dir.join("long.html"), "<style>body{margin:0}p{margin:0;height:900px}</style><p>one</p><p>two</p>").unwrap();
        let long = Url::from_file_path(dir.join("long.html")).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        tab.open(long.clone());
        assert!(deliver(&mut tab, &rx));
        tab.scroll = 100.0;
        tab.open(b.clone());
        assert!(deliver(&mut tab, &rx));
        tab.scroll = 55.0;
        tab.open(long.clone());
        assert!(deliver(&mut tab, &rx));
        tab.scroll = 900.0;
        tab.go_back();
        assert_eq!((tab.page.url.clone(), tab.scroll), (b.clone(), 55.0));
        tab.go_back();
        assert_eq!((tab.page.url.clone(), tab.scroll), (long.clone(), 100.0), "the first visit's position, not the second's");
        tab.go_forward();
        tab.go_forward();
        assert_eq!(tab.scroll, 900.0);
        // lots of fragment visits on one page: the recent ones all still come back right
        for i in 0..250 {
            tab.scroll = 700.0;
            tab.follow(&format!("#f{i}"));
        }
        for i in 0..100 {
            tab.go_back();
            assert_eq!(tab.scroll, 700.0, "step back {i}");
        }
    }

    #[test]
    fn a_page_loaded_with_a_fragment_scrolls_there_once_it_is_laid_out() {
        let (dir, _a, _b) = site("fragment");
        std::fs::write(dir.join("long.html"), "<style>body{margin:0}p{margin:0;height:500px}</style><p>one</p><h2 id=sec style='margin:0'>S</h2><p>two</p>").unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        let target = Url::from_file_path(dir.join("long.html")).unwrap();
        let mut with_fragment = target.clone();
        with_fragment.set_fragment(Some("sec"));
        tab.open(with_fragment);
        assert!(deliver(&mut tab, &rx));
        // before layout there is nothing to scroll to, and nothing is lost
        tab.settle_fragment();
        assert_eq!(tab.scroll, 0.0);
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert_eq!(tab.scroll, 500.0);
        // it only happens once: the user's own scrolling is not overridden by a later relayout
        tab.scroll = 10.0;
        tab.settle_fragment();
        assert_eq!(tab.scroll, 10.0);
        // a fragment that matches nothing is ignored
        let mut missing = target.clone();
        missing.set_fragment(Some("zzz"));
        tab.open(missing);
        assert!(deliver(&mut tab, &rx));
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert_eq!(tab.scroll, 0.0);
        // a reload remembers where you were rather than jumping back to the fragment
        tab.scroll = 123.0;
        tab.reload();
        assert!(deliver(&mut tab, &rx));
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert_eq!(tab.scroll, 123.0);
    }

    #[test]
    fn a_redirect_that_names_no_fragment_keeps_the_requested_one() {
        // `/old#sec` answered with `302 Location: /new` lands on `/new` (no fragment in the
        // final URL we get back), but the visit is still to #sec
        let (dir, _a, _b) = site("redirect-fragment");
        std::fs::write(dir.join("new.html"), "<style>body{margin:0}p{margin:0;height:500px}</style><p>one</p><h2 id=sec style='margin:0'>S</h2>").unwrap();
        let landed = Url::from_file_path(dir.join("new.html")).unwrap();
        let (notify, rx) = queue();
        let mut tab = Tab::blank(1, notify);
        let mut asked = landed.clone();
        asked.set_fragment(Some("sec"));
        tab.open(asked);
        let UserEvent::Loaded { ticket, .. } = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        // hand over the page as the redirect would have built it: its URL has no fragment
        assert!(tab.finish_load(ticket, Page::load(&landed)));
        assert_eq!(tab.page.url.fragment(), None);
        tab.page.relayout(&crate::fonts::Fonts::load(), 400.0);
        tab.laid_out_width = 400.0;
        tab.settle_fragment();
        assert_eq!(tab.scroll, 500.0);
    }

    #[test]
    fn a_page_built_on_another_thread_arrives_intact() {
        // What the loader thread does, end to end: build a page with images and styles
        // on one thread, hand it over whole, and use it on this one.
        let dir = std::env::temp_dir().join(format!("mybrowser-bg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut png = vec![];
        let mut enc = png::Encoder::new(&mut png, 3, 2);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&[255u8; 24]).unwrap();
        std::fs::write(dir.join("a.png"), &png).unwrap();
        std::fs::write(dir.join("p.html"), "<title>Threaded</title><style>p{color:red}</style><p>hi</p><img src=a.png><img src=a.png><form><input name=q value=x></form>").unwrap();
        let url = Url::from_file_path(dir.join("p.html")).unwrap();

        let (tx, rx) = std::sync::mpsc::channel::<(u64, SendPage)>();
        let handles: Vec<_> = (0..4u64)
            .map(|i| {
                let (tx, url) = (tx.clone(), url.clone());
                std::thread::spawn(move || tx.send((i, SendPage(Page::load(&url)))).unwrap())
            })
            .collect();
        drop(tx);
        for h in handles {
            h.join().unwrap();
        }
        let fonts = crate::fonts::Fonts::load();
        let mut seen = 0;
        for (_, SendPage(mut page)) in rx {
            seen += 1;
            assert_eq!((page.title.as_str(), page.images.len()), ("Threaded", 2));
            page.relayout(&fonts, 300.0); // lays out and clones the Rc'd images, on this thread
            assert!(page.layout.items.iter().any(|i| matches!(i, crate::layout::Item::Image { .. })));
            assert_eq!(page.layout.controls.len(), 1, "form controls survive the handoff too");
        }
        assert_eq!(seen, 4);
    }
}
