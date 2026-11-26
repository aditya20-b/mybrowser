//! One browser tab: its page, history, scroll position and the navigation in
//! flight. Everything here is independent of the window system (loaders report
//! back through a plain callback), so it is unit-tested without opening one.
use crate::dom::NodeId;
use crate::forms::{self, ControlKind};
use crate::net;
use crate::page::Page;
use std::sync::Arc;
use url::Url;

/// What a clicked `href` means.
#[derive(Debug, PartialEq)]
pub enum Link {
    /// Nothing to do: empty href, an in-page `#anchor`, or an unparseable URL.
    Nothing,
    /// A scheme we don't handle (`mailto:`, `javascript:`, `tel:`, ...) or
    /// something a page isn't allowed to open (a web page linking to `file:`).
    External(String),
    Navigate(Url),
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

pub fn resolve_link(base: &Url, href: &str) -> Link {
    let href = href.trim();
    if href.is_empty() || href.starts_with('#') {
        return Link::Nothing;
    }
    let Ok(target) = base.join(href) else { return Link::Nothing };
    match target.scheme() {
        // web pages must not be able to send you to local files
        "file" if matches!(base.scheme(), "http" | "https") => Link::External(target.to_string()),
        "http" | "https" | "file" => {
            if strip_fragment(&target) == strip_fragment(base) {
                Link::Nothing // same document, different fragment: no reload
            } else {
                Link::Navigate(target)
            }
        }
        _ => Link::External(target.to_string()),
    }
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
    /// Form data for the next `begin_load` to POST (set by `submit_form`).
    next_post: Option<String>,
    /// Form data of the navigation in flight, so restarting it (reload while
    /// loading) POSTs again instead of silently turning into a GET.
    inflight_post: Option<String>,
    /// The page the next `begin_load` was started from (a clicked link or a
    /// submitted form); `None` for typed addresses and history. Decides `SameSite` cookies.
    next_initiator: Option<Url>,
    inflight_initiator: Option<Url>,
    notify: Notify,
}

impl Tab {
    /// A new tab that starts loading `start` (started from `initiator`, if a page opened it).
    pub fn new(id: u64, start: Url, initiator: Option<Url>, notify: Notify) -> Tab {
        let mut tab = Tab::blank(id, notify);
        tab.page = Page::from_source(&start, "<p style='color:#666'>Loading…</p>");
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
            next_post: None,
            inflight_post: None,
            next_initiator: None,
            inflight_initiator: None,
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
    pub fn finish_load(&mut self, ticket: u64, page: Page) -> bool {
        let Some(pending) = self.nav.finish(ticket) else { return false };
        self.inflight_post = None;
        self.inflight_initiator = None;
        self.history.apply(pending.hist, pending.from, &page.url);
        self.page = page;
        self.laid_out_width = -1.0; // force relayout on the next redraw
        self.scroll = pending.keep_scroll.unwrap_or(0.0); // clamped on the next redraw
        true
    }

    /// Give up on the load in flight. Returns whether there was one.
    pub fn cancel_load(&mut self) -> bool {
        let was = self.nav.cancel();
        if was {
            self.inflight_post = None;
            self.inflight_initiator = None;
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

    pub fn go_back(&mut self) {
        if let Some(prev) = self.history.back_target().cloned() {
            self.begin_load(prev, Hist::Back, None);
        }
    }

    pub fn go_forward(&mut self) {
        if let Some(next) = self.history.forward_target().cloned() {
            self.begin_load(next, Hist::Forward, None);
        }
    }

    /// Follow a link on this page. Returns whether a navigation started.
    pub fn follow(&mut self, href: &str) -> bool {
        match resolve_link(&self.page.url, href) {
            Link::Nothing => false, // in-page anchors aren't scrolled to yet
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
        match resolve_link(&self.page.url, href) {
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
        if forms::is_disabled(&self.page.dom, id) {
            return Ok(());
        }
        let kind = forms::kind_of(&self.page.dom, id);
        let mut result = Ok(());
        {
            let Page { dom, form, .. } = &mut self.page;
            match kind {
                Some(ControlKind::Text | ControlKind::Password | ControlKind::Textarea | ControlKind::Button) => form.focus = Some(id),
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
        let Some(sub) = self.page.form.submit(&self.page.dom, form_el, submitter, &self.page.url) else { return Ok(()) };
        let post = matches!(sub.method, forms::Method::Post);
        // pressing Enter twice must not send a POST twice (a duplicate order, say)
        if post && self.nav.is_loading() && self.inflight_post.is_some() {
            return Ok(());
        }
        let from = self.page.url.clone();
        let target = match resolve_link(&from, sub.url.as_str()) {
            Link::Navigate(url) => url,
            Link::Nothing => from.clone(), // submitting to the page you're already on
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
        if target == from {
            // replace the page in place (not `reload`, which might restart a different pending load)
            self.begin_load(target, Hist::Replace, None);
        } else {
            self.open(target);
        }
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
        for nothing in ["", "  ", "#section", "page.html?q=1#other"] {
            assert_eq!(resolve_link(&page, nothing), Link::Nothing, "{nothing:?}");
        }
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
    fn form_submissions_go_through_the_same_link_safety_checks() {
        // a form on a web page can't send you to a local file or a javascript: URL
        let page = u("https://example.com/form.html");
        for action in ["file:///etc/passwd", "javascript:alert(1)", "mailto:a@b.c"] {
            assert!(matches!(resolve_link(&page, action), Link::External(_)), "{action}");
        }
        // and a GET form to the same URL is "nothing new", which the window turns into a reload
        assert_eq!(resolve_link(&page, "https://example.com/form.html"), Link::Nothing);
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
        assert!(deliver(&mut tab, &rx));
        assert_eq!((tab.title().as_str(), tab.history.forward.clone()), ("Page A", vec![b.clone()]));
        tab.go_forward();
        assert!(deliver(&mut tab, &rx));
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
