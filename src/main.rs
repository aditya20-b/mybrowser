mod chrome;
mod css;
mod dom;
mod fonts;
mod forms;
mod html;
mod images;
mod layout;
mod net;
mod page;
mod paint;
mod style;

use chrome::{AddressBar, ChromeView, Hit};
use dom::NodeId;
use fonts::Fonts;
use forms::{ControlKind, FormState};
use page::Page;
use paint::Painter;
use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

const USAGE: &str = "usage: mybrowser [URL or file]\n       mybrowser --screenshot OUT.png [--width PX] [--scale N] [URL or file]";

fn die(msg: &str) -> ! {
    eprintln!("mybrowser: {msg}\n{USAGE}");
    std::process::exit(2)
}

/// Remove `--name VALUE` from `args`. A flag without a value (or followed by
/// another flag) is an error rather than silently swallowing the next argument.
fn take_flag(args: &mut Vec<String>, name: &str) -> Result<Option<String>, String> {
    let Some(i) = args.iter().position(|a| a == name) else { return Ok(None) };
    args.remove(i);
    match args.get(i) {
        Some(v) if !v.starts_with("--") => Ok(Some(args.remove(i))),
        _ => Err(format!("{name} needs a value")),
    }
}

/// Parse a finite number in `[min, max]` (rejects NaN, infinities and out-of-range values).
fn parse_number(name: &str, value: &str, min: f32, max: f32) -> Result<f32, String> {
    let n: f32 = value.parse().map_err(|_| format!("{name}: '{value}' is not a number"))?;
    if !n.is_finite() || n < min || n > max {
        return Err(format!("{name} must be a number between {min} and {max}"));
    }
    Ok(n)
}

/// Is `host` (no port) exactly `localhost` or an IPv4 loopback address?
/// Prefix matching would treat `localhost.evil.com` or `127.evil.com` as local
/// and send the visit to a remote server in cleartext.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// Turn whatever the user typed (or passed on the command line) into a URL:
/// a full http/https/file/data URL, an existing file path, or a bare host such
/// as `example.com/page` (which gets `https://`, or `http://` for loopback).
fn url_from_input(input: &str, cwd: &Path) -> Result<Url, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty address".into());
    }
    if let Ok(u) = Url::parse(s) {
        if matches!(u.scheme(), "http" | "https" | "file" | "data") {
            return Ok(u);
        }
    }
    let path = cwd.join(s);
    if path.exists() {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        return Url::from_file_path(&path).map_err(|_| format!("bad path {}", path.display()));
    }
    // A bare host: must look like one (a dot or loopback, an optional numeric
    // port, no credentials), so `mailto:a@b.c` is rejected instead of being
    // misread as https://b.c with a username.
    let authority = s.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (authority, None),
    };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let local = is_loopback_host(host);
    let looks_like_host = !authority.contains('@') && !authority.contains(' ') && (authority.contains('.') || local) && port_ok;
    if looks_like_host {
        let scheme = if local { "http" } else { "https" };
        return Url::parse(&format!("{scheme}://{s}")).map_err(|e| format!("{s}: {e}"));
    }
    Err(format!("'{s}' is not a URL or an existing file"))
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let screenshot = take_flag(&mut args, "--screenshot").unwrap_or_else(|e| die(&e));
    let width = match take_flag(&mut args, "--width").unwrap_or_else(|e| die(&e)) {
        Some(v) => parse_number("--width", &v, 1.0, 16_384.0).unwrap_or_else(|e| die(&e)),
        None => 800.0,
    };
    let scale = match take_flag(&mut args, "--scale").unwrap_or_else(|e| die(&e)) {
        Some(v) => parse_number("--scale", &v, 0.25, 8.0).unwrap_or_else(|e| die(&e)),
        None => 1.0,
    };
    if let Some(unknown) = args.iter().find(|a| a.starts_with("--")) {
        die(&format!("unknown option {unknown}"));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|e| die(&format!("cannot read the current directory: {e}")));
    let start = url_from_input(args.first().map(|s| s.as_str()).unwrap_or("pages/index.html"), &cwd)
        .unwrap_or_else(|e| die(&e));

    let fonts = Fonts::load();
    if let Some(out) = screenshot {
        screenshot_to_png(&start, &fonts, width, scale, Path::new(&out));
        return;
    }

    let event_loop = EventLoop::<UserEvent>::with_user_event().build().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(fonts, start, cwd, event_loop.create_proxy());
    event_loop.run_app(&mut app).unwrap();
}

/// Headless mode: render the whole page to a PNG (handy for testing).
/// This is the page only, without the toolbar.
fn screenshot_to_png(url: &Url, fonts: &Fonts, width: f32, scale: f32, out: &Path) {
    let t0 = std::time::Instant::now();
    let mut page = Page::load(url);
    let t_load = t0.elapsed();
    page.relayout(fonts, width);
    let t_layout = t0.elapsed() - t_load;
    // Cap the image so a gigantic page can't allocate gigabytes: at most 16000
    // logical px tall and 2^26 pixels in total (about 270 MB of u32s).
    let w = ((width * scale).round() as usize).max(1);
    let max_h = (1usize << 26) / w;
    let h = ((page.layout.height.min(16_000.0) * scale).ceil() as usize).clamp(1, max_h.max(1));
    let mut buf = vec![0u32; w * h];
    Painter::default().paint(&mut buf, w, h, &page.layout, fonts, 0.0, scale);
    let t_paint = t0.elapsed() - t_load - t_layout;
    eprintln!("fetch+parse+style {t_load:?}, layout {t_layout:?}, paint {t_paint:?}");
    let rgb: Vec<u8> = buf.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    let file = std::fs::File::create(out).unwrap_or_else(|e| {
        eprintln!("mybrowser: cannot create {}: {e}", out.display());
        std::process::exit(1)
    });
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let written = enc.write_header().and_then(|mut wr| wr.write_image_data(&rgb));
    if let Err(e) = written {
        eprintln!("mybrowser: could not write {}: {e}", out.display());
        std::process::exit(1);
    }
    println!("wrote {} ({w}x{h}); {} display items", out.display(), page.layout.items.len());
}

/// What a clicked `href` means.
#[derive(Debug, PartialEq)]
enum Link {
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

fn resolve_link(base: &Url, href: &str) -> Link {
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

/// A fully built page on its way from a loader thread to the UI thread.
///
/// `Page` holds `Rc<Image>`s, which are not `Send`. That is only a problem if
/// two threads could touch the same reference count, and here they can't: every
/// clone of every `Rc` lives inside this one `Page` (the loader's temporary
/// caches are dropped before it returns), and the loader thread gives the whole
/// value away through a channel without keeping anything. Ownership moves as a
/// unit, so the counts are only ever touched by one thread at a time.
struct SendPage(Page);
unsafe impl Send for SendPage {}

enum UserEvent {
    Loaded { ticket: u64, page: SendPage },
}

/// How a finished navigation changes the history stacks.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hist {
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
struct History {
    back: Vec<Url>,
    forward: Vec<Url>,
}

impl History {
    /// The page the Back / Forward button would go to.
    fn back_target(&self) -> Option<&Url> {
        self.back.last()
    }

    fn forward_target(&self) -> Option<&Url> {
        self.forward.last()
    }

    /// A navigation finished: `from` is the page that was on screen, `landed` the
    /// page that replaced it (after any redirects).
    fn apply(&mut self, how: Hist, from: Url, landed: &Url) {
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
struct Pending {
    ticket: u64,
    /// What is being loaded.
    url: Url,
    hist: Hist,
    /// The page on screen when this started (the one a successful load replaces).
    from: Url,
    /// Restore this scroll offset when it lands (reload keeps your place).
    keep_scroll: Option<f32>,
}

/// Bookkeeping for the one navigation in flight. Each start gets a ticket;
/// only the newest ticket may complete, so a slow, superseded load can never
/// replace a page the user has since moved on to.
#[derive(Default)]
struct Navigation {
    ticket: u64,
    pending: Option<Pending>,
}

impl Navigation {
    fn start(&mut self, from: Url, url: Url, hist: Hist, keep_scroll: Option<f32>) -> u64 {
        self.ticket += 1;
        self.pending = Some(Pending { ticket: self.ticket, url, hist, from, keep_scroll });
        self.ticket
    }

    /// A load finished: returns its details if it is still the current one.
    fn finish(&mut self, ticket: u64) -> Option<Pending> {
        if ticket == self.ticket { self.pending.take() } else { None }
    }

    /// Abandon whatever is loading. Returns whether something was.
    fn cancel(&mut self) -> bool {
        self.ticket += 1;
        self.pending.take().is_some()
    }

    fn pending(&self) -> Option<&Pending> {
        self.pending.as_ref()
    }

    fn is_loading(&self) -> bool {
        self.pending.is_some()
    }
}

struct App {
    fonts: Fonts,
    painter: Painter,
    page: Page,
    bar: AddressBar,
    cwd: std::path::PathBuf,
    history: History,
    nav: Navigation,
    proxy: EventLoopProxy<UserEvent>,
    scroll: f32,
    laid_out_width: f32,
    cursor: (f32, f32), // physical pixels
    hover: Option<String>,
    modifiers: ModifiersState,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    fn new(fonts: Fonts, start: Url, cwd: std::path::PathBuf, proxy: EventLoopProxy<UserEvent>) -> App {
        let page = Page::from_source(&start, "<p style='color:#666'>Loading…</p>");
        let bar = AddressBar::new(start.as_str());
        let mut app = App {
            fonts, painter: Painter::default(), page, bar, cwd, history: History::default(),
            nav: Navigation::default(), proxy, scroll: 0.0, laid_out_width: -1.0, cursor: (0.0, 0.0),
            hover: None, modifiers: ModifiersState::empty(), window: None, surface: None,
        };
        app.begin_load(start, Hist::Replace, None);
        app
    }

    fn scale(&self) -> f32 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32)
    }

    /// Window size in logical pixels.
    fn window_size(&self) -> (f32, f32) {
        let s = self.scale();
        self.window.as_ref().map_or((0.0, 0.0), |w| {
            let p = w.inner_size();
            (p.width as f32 / s, p.height as f32 / s)
        })
    }

    /// Height of the area the page is drawn in (the window minus the toolbar).
    fn view_height(&self) -> f32 {
        (self.window_size().1 - chrome::HEIGHT).max(0.0)
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn set_title(&self) {
        if let Some(w) = &self.window {
            let title = if self.nav.is_loading() { "Loading…".to_string() } else { self.page.display_title() };
            w.set_title(&format!("{title} — mybrowser"));
        }
    }

    fn clamp_scroll(&mut self) {
        let max = (self.page.layout.height - self.view_height()).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    /// Recompute what is under the mouse and set the cursor icon. Must run
    /// whenever the page moves under a stationary cursor: scrolling, relayout,
    /// navigation.
    fn update_hover(&mut self) {
        let s = self.scale();
        let (cx, cy) = (self.cursor.0 / s, self.cursor.1 / s);
        let (icon, hover) = if cy < chrome::HEIGHT {
            let icon = match chrome::hit_test(cx, cy, self.window_size().0) {
                Some(Hit::Address) => CursorIcon::Text,
                Some(_) => CursorIcon::Pointer,
                None => CursorIcon::Default,
            };
            (icon, None)
        } else {
            let (px, py) = (cx, cy - chrome::HEIGHT + self.scroll);
            if let Some(id) = self.page.control_at(px, py) {
                // an I-beam over text fields, a hand over buttons, boxes and menus
                let text = forms::kind_of(&self.page.dom, id).is_some_and(forms::is_text_like);
                (if text { CursorIcon::Text } else { CursorIcon::Pointer }, None)
            } else {
                let link = self.link_at(px, py);
                (if link.is_some() { CursorIcon::Pointer } else { CursorIcon::Default }, link)
            }
        };
        self.hover = hover;
        if let Some(w) = &self.window {
            w.set_cursor(icon);
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll += dy;
        self.clamp_scroll();
        self.update_hover();
        self.request_redraw();
    }

    /// Start loading `url` on a background thread. The current page stays on
    /// screen and responsive until the new one arrives; starting another load (or
    /// pressing Escape) abandons this one. The history stacks are only touched
    /// when the load lands (see `finish_load`).
    fn begin_load(&mut self, url: Url, hist: Hist, keep_scroll: Option<f32>) {
        let ticket = self.nav.start(self.page.url.clone(), url.clone(), hist, keep_scroll);
        self.bar.show(url.as_str());
        self.set_title();
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            // A panic while loading must still end the "Loading…" state, even if
            // building the error page itself panics.
            let page = std::panic::catch_unwind(|| Page::load(&url))
                .or_else(|_| std::panic::catch_unwind(|| Page::error(&url, "internal error while loading this page")))
                .unwrap_or_else(|_| Page::from_source(&url, "<p>Could not load this page.</p>"));
            let _ = proxy.send_event(UserEvent::Loaded { ticket, page: SendPage(page) });
        });
        self.request_redraw();
    }

    /// A loader thread delivered a page.
    fn finish_load(&mut self, ticket: u64, page: Page) {
        let Some(pending) = self.nav.finish(ticket) else { return }; // superseded or cancelled
        self.history.apply(pending.hist, pending.from, &page.url);
        self.page = page;
        self.bar.show(self.page.url.as_str()); // the final URL, after redirects
        self.laid_out_width = -1.0; // force relayout on the next redraw
        self.scroll = pending.keep_scroll.unwrap_or(0.0); // clamped on the next redraw
        self.update_hover();
        self.set_title();
        self.request_redraw();
    }

    /// Give up on the load in flight and show the current page's address again.
    fn cancel_load(&mut self) {
        if self.nav.cancel() {
            let url = self.page.url.to_string();
            self.bar.show(&url);
            self.set_title();
            self.request_redraw();
        }
    }

    fn open(&mut self, url: Url) {
        self.begin_load(url, Hist::Push, None);
    }

    /// Reload. While a navigation is loading, that navigation is restarted (not
    /// silently replaced by a reload of the page underneath it).
    fn reload(&mut self) {
        if let Some(p) = self.nav.pending() {
            let (url, hist) = (p.url.clone(), p.hist);
            self.begin_load(url, hist, None);
        } else {
            let (url, keep) = (self.page.url.clone(), self.scroll);
            self.begin_load(url, Hist::Replace, Some(keep));
        }
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.history.back_target().cloned() {
            self.begin_load(prev, Hist::Back, None);
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.history.forward_target().cloned() {
            self.begin_load(next, Hist::Forward, None);
        }
    }

    fn follow(&mut self, href: &str) {
        match resolve_link(&self.page.url, href) {
            Link::Nothing => {} // in-page anchors aren't scrolled to yet
            Link::External(url) => eprintln!("not supported (yet): {url}"),
            Link::Navigate(url) => self.open(url),
        }
    }

    fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let l = self.page.layout.links.iter().find(|l| x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h);
        l.map(|l| l.href.clone())
    }

    // ---------- forms ----------

    /// The form state changed (focus, text, a toggle): the page must be laid out
    /// and drawn again.
    fn form_changed(&mut self) {
        self.laid_out_width = -1.0;
        self.request_redraw();
    }

    /// A click landed on control `id`.
    fn activate_control(&mut self, id: NodeId) {
        let kind = forms::kind_of(&self.page.dom, id);
        let Page { dom, form, .. } = &mut self.page;
        match kind {
            Some(ControlKind::Text | ControlKind::Password | ControlKind::Textarea) => form.focus = Some(id),
            Some(ControlKind::Checkbox | ControlKind::Radio) => {
                form.focus = Some(id);
                form.toggle(dom, id);
            }
            Some(ControlKind::Select) => {
                form.focus = Some(id);
                form.cycle_select(dom, id);
            }
            Some(ControlKind::Submit) => {
                form.focus = Some(id);
                self.submit_form(id, Some(id));
            }
            Some(ControlKind::Button) => form.focus = Some(id),
            _ => {}
        }
        self.form_changed();
    }

    /// Submit the form that contains `control`, as if `submitter` (a button) was clicked.
    fn submit_form(&mut self, control: NodeId, submitter: Option<NodeId>) {
        let Some(form_el) = forms::form_of(&self.page.dom, control) else { return };
        let Some(sub) = self.page.form.submit(&self.page.dom, form_el, submitter, &self.page.url) else { return };
        match sub.method {
            forms::Method::Get => match resolve_link(&self.page.url, sub.url.as_str()) {
                Link::Navigate(url) => self.open(url),
                Link::Nothing => self.reload(), // submitting the page you're already on
                Link::External(url) => eprintln!("not supported (yet): {url}"),
            },
            forms::Method::Post => eprintln!("POST forms aren't supported yet: {}", sub.url),
        }
    }

    /// A key press while a form control has focus. Returns true if the control
    /// consumed it. Typing in a text field consumes everything (so `r` types an
    /// `r` instead of reloading); other controls only take Space, Enter, Tab and Escape.
    fn control_key(&mut self, event: &KeyEvent, cmd: bool) -> bool {
        let Some(id) = self.page.form.focus else { return false };
        if cmd {
            return false; // Cmd+L, Cmd+R ... still work
        }
        let kind = forms::kind_of(&self.page.dom, id);
        let text_like = kind.is_some_and(forms::is_text_like);
        let shift = self.modifiers.shift_key();
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.page.form.focus = None;
                self.form_changed();
                true
            }
            Key::Named(NamedKey::Tab) => {
                let Page { dom, form, .. } = &mut self.page;
                form.focus_next(dom, !shift);
                self.form_changed();
                true
            }
            Key::Named(NamedKey::Enter) => {
                match kind {
                    Some(ControlKind::Textarea) => self.page.dom_value_edit(id, "\n"),
                    Some(ControlKind::Submit) => self.submit_form(id, Some(id)),
                    Some(ControlKind::Text | ControlKind::Password) => {
                        // implicit submission: Enter "clicks" the form's first submit button
                        if let Some(form_el) = forms::form_of(&self.page.dom, id) {
                            let button = FormState::default_button(&self.page.dom, form_el);
                            self.submit_form(id, button);
                        }
                    }
                    Some(ControlKind::Checkbox | ControlKind::Radio | ControlKind::Select) => self.activate_control(id),
                    _ => {}
                }
                self.form_changed();
                true
            }
            Key::Named(NamedKey::Backspace) if text_like => {
                let Page { dom, form, .. } = &mut self.page;
                form.backspace(dom, id);
                self.form_changed();
                true
            }
            Key::Named(NamedKey::Space)
                if matches!(kind, Some(ControlKind::Checkbox | ControlKind::Radio | ControlKind::Select | ControlKind::Submit | ControlKind::Button)) =>
            {
                self.activate_control(id);
                true
            }
            _ if text_like => {
                if let Some(text) = &event.text {
                    self.page.dom_value_edit(id, text.as_str());
                }
                self.form_changed();
                true
            }
            _ => false,
        }
    }

    /// Enter pressed in the address bar. Going to the page you're already on is
    /// a reload, not a new history entry.
    fn commit_address(&mut self) {
        let typed = self.bar.submit_text().to_string();
        match url_from_input(&typed, &self.cwd) {
            Ok(url) if url == self.page.url && !self.nav.is_loading() => self.reload(),
            Ok(url) => self.open(url),
            Err(e) => self.bar.error = Some(e),
        }
    }

    /// Left click released at the current cursor position.
    fn click(&mut self) {
        let s = self.scale();
        let (cx, cy) = (self.cursor.0 / s, self.cursor.1 / s);
        if cy < chrome::HEIGHT {
            match chrome::hit_test(cx, cy, self.window_size().0) {
                Some(Hit::Back) => self.go_back(),
                Some(Hit::Forward) => self.go_forward(),
                Some(Hit::Reload) => self.reload(),
                Some(Hit::Address) => {
                    if self.bar.is_focused() {
                        self.bar.click_again();
                    } else {
                        self.bar.focus();
                    }
                }
                None => {}
            }
        } else {
            // clicking the page ends address editing
            if self.bar.is_focused() {
                let url = if self.nav.is_loading() { self.bar.submit_text().to_string() } else { self.page.url.to_string() };
                self.bar.show(&url);
            }
            let (px, py) = (cx, cy - chrome::HEIGHT + self.scroll);
            if let Some(id) = self.page.control_at(px, py) {
                self.activate_control(id);
            } else {
                // clicking anywhere else leaves the control that had focus
                if self.page.form.focus.take().is_some() {
                    self.form_changed();
                }
                self.update_hover(); // hit-test at release time rather than trusting a stale hover
                if let Some(href) = self.hover.clone() {
                    self.follow(&href);
                }
            }
        }
        self.request_redraw();
    }

    /// A key pressed while the address bar has focus.
    fn address_key(&mut self, event: &KeyEvent, cmd: bool) {
        match &event.logical_key {
            Key::Named(NamedKey::Enter) => self.commit_address(),
            Key::Named(NamedKey::Escape) => {
                let url = self.page.url.to_string();
                self.bar.show(&url);
                self.cancel_load();
            }
            Key::Named(NamedKey::Backspace) if cmd => self.bar.clear(),
            Key::Named(NamedKey::Backspace) => self.bar.backspace(),
            Key::Named(NamedKey::Delete) => self.bar.delete(),
            Key::Named(NamedKey::ArrowLeft) if cmd => self.bar.home(),
            Key::Named(NamedKey::ArrowRight) if cmd => self.bar.end(),
            Key::Named(NamedKey::ArrowLeft) => self.bar.left(),
            Key::Named(NamedKey::ArrowRight) => self.bar.right(),
            Key::Named(NamedKey::Home) => self.bar.home(),
            Key::Named(NamedKey::End) => self.bar.end(),
            Key::Character(c) if cmd => match c.as_str().to_ascii_lowercase().as_str() {
                "a" => self.bar.select_all(),
                "l" => self.bar.focus(),
                "v" => {
                    if let Some(text) = chrome::clipboard_get() {
                        self.bar.insert(&text);
                    }
                }
                "c" => chrome::clipboard_set(self.bar.text()),
                "x" => {
                    chrome::clipboard_set(self.bar.text());
                    self.bar.clear();
                }
                _ => {}
            },
            _ => {
                if !cmd {
                    if let Some(text) = &event.text {
                        self.bar.insert(text.as_str());
                    }
                }
            }
        }
    }

    fn on_key(&mut self, event: &KeyEvent) {
        let cmd = self.modifiers.super_key() || self.modifiers.control_key();
        if self.bar.is_focused() {
            self.address_key(event, cmd);
            self.request_redraw();
            return;
        }
        if self.control_key(event, cmd) {
            self.request_redraw();
            return;
        }
        let page = self.view_height() * 0.9;
        let alt_or_cmd = self.modifiers.alt_key() || self.modifiers.super_key();
        match &event.logical_key {
            Key::Character(c) if cmd => match c.as_str().to_ascii_lowercase().as_str() {
                "l" => self.bar.focus(),
                "r" => self.reload(),
                "[" => self.go_back(),
                "]" => self.go_forward(),
                "." => self.cancel_load(),
                _ => {}
            },
            Key::Named(NamedKey::Escape) => self.cancel_load(),
            Key::Named(NamedKey::Tab) => {
                // Tab into the page's first control (Shift+Tab: the last)
                let (shift, Page { dom, form, .. }) = (self.modifiers.shift_key(), &mut self.page);
                form.focus_next(dom, !shift);
                self.form_changed();
            }
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(48.0),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(-48.0),
            Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => self.scroll_by(page),
            Key::Named(NamedKey::PageUp) => self.scroll_by(-page),
            Key::Named(NamedKey::Home) => self.scroll_by(-1e9),
            Key::Named(NamedKey::End) => self.scroll_by(1e9),
            Key::Named(NamedKey::ArrowLeft) if alt_or_cmd => self.go_back(),
            Key::Named(NamedKey::ArrowRight) if alt_or_cmd => self.go_forward(),
            Key::Named(NamedKey::Backspace) if self.modifiers.shift_key() => self.go_forward(),
            Key::Named(NamedKey::Backspace) => self.go_back(),
            Key::Named(NamedKey::F5) => self.reload(),
            Key::Character(c) if c.eq_ignore_ascii_case("r") => self.reload(),
            _ => {}
        }
        self.request_redraw();
    }

    fn redraw(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else { return };
        let scale = self.scale();
        let logical_w = size.width as f32 / scale;
        if (logical_w - self.laid_out_width).abs() > 0.5 {
            self.page.relayout(&self.fonts, logical_w);
            self.laid_out_width = logical_w;
        }
        // The page height or the window height may have changed (resize, reload,
        // navigation), so the clamp and the hover state are refreshed every frame.
        self.clamp_scroll();
        self.update_hover();

        let (w_px, h_px) = (size.width as usize, size.height as usize);
        let top = ((chrome::HEIGHT * scale).round() as usize).min(h_px);
        let surface = self.surface.as_mut().unwrap();
        surface.resize(w, h).unwrap();
        let mut buffer = surface.buffer_mut().unwrap();
        let pixels: &mut [u32] = &mut buffer;
        // The page is painted into the rows below the toolbar, then the toolbar on top.
        self.painter.paint(&mut pixels[top * w_px..], w_px, h_px - top, &self.page.layout, &self.fonts, self.scroll, scale);
        let view = ChromeView {
            bar: &self.bar,
            can_back: self.history.back_target().is_some(),
            can_forward: self.history.forward_target().is_some(),
        };
        self.painter.paint_chrome(pixels, w_px, h_px, &self.fonts, scale, &view);
        buffer.present().unwrap();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Loading… — mybrowser")
            .with_inner_size(LogicalSize::new(960.0, 720.0));
        let window = Rc::new(el.create_window(attrs).unwrap());
        let context = softbuffer::Context::new(window.clone()).unwrap();
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).unwrap());
        self.window = Some(window);
        self.set_title();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Loaded { ticket, page } => self.finish_load(ticket, page.0),
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => self.request_redraw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => -y * 48.0,
                    MouseScrollDelta::PixelDelta(p) => -(p.y as f32) / self.scale(),
                };
                self.scroll_by(dy);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                self.update_hover();
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => self.click(),
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => self.on_key(&event),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn flags_require_values_and_never_swallow_other_flags() {
        let mut a = args(&["page.html", "--screenshot", "o.png"]);
        assert_eq!(take_flag(&mut a, "--screenshot"), Ok(Some("o.png".into())));
        assert_eq!(a, ["page.html"]);
        assert_eq!(take_flag(&mut a, "--width"), Ok(None));
        // `--screenshot` with no value used to silently open the GUI
        assert!(take_flag(&mut args(&["--screenshot"]), "--screenshot").is_err());
        // and used to swallow the next flag as its value
        let mut a = args(&["--width", "--scale", "2"]);
        assert!(take_flag(&mut a, "--width").is_err());
    }

    #[test]
    fn numbers_must_be_finite_and_in_range() {
        assert_eq!(parse_number("--width", "800", 1.0, 16384.0), Ok(800.0));
        for bad in ["0", "-5", "NaN", "inf", "-inf", "1e7", "abc", "", "16385"] {
            assert!(parse_number("--width", bad, 1.0, 16384.0).is_err(), "{bad}");
        }
    }

    #[test]
    fn address_parsing() {
        let dir = std::env::temp_dir().join(format!("mybrowser-addr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("page.html"), "x").unwrap();
        let go = |s: &str| url_from_input(s, &dir);

        assert_eq!(go("https://example.com/a?b=1#c"), Ok(u("https://example.com/a?b=1#c")));
        assert_eq!(go("  http://example.com  "), Ok(u("http://example.com/")));
        assert_eq!(go("example.com/path"), Ok(u("https://example.com/path")));
        assert_eq!(go("localhost:8080/x"), Ok(u("http://localhost:8080/x")));
        assert_eq!(go("LOCALHOST"), Ok(u("http://localhost/")));
        assert_eq!(go("127.0.0.1:3000"), Ok(u("http://127.0.0.1:3000/")));
        assert_eq!(go("data:text/html,hi"), Ok(u("data:text/html,hi")));
        let file = go("page.html").unwrap();
        assert_eq!(file.scheme(), "file");
        assert!(file.path().ends_with("/page.html"));
        assert_eq!(go(file.as_str()), Ok(file));

        // things that are not addresses must not be navigated anywhere surprising
        for bad in ["", "   ", "javascript:alert(1)", "mailto:a@b.c", "user:pw@evil.com", "ftp://x.com/y", "nosuchfile", "two words.com", "host:notaport.com", "localhostfoo"] {
            assert!(go(bad).is_err(), "{bad:?} -> {:?}", go(bad));
        }
    }

    #[test]
    fn look_alike_local_hosts_get_https_not_cleartext_http() {
        // These used to match a `starts_with("localhost")` / `starts_with("127.")` check
        // and be sent over plain HTTP to a remote server.
        let dir = std::env::temp_dir();
        for (typed, expected) in [
            ("localhost.evil.com/login", "https://localhost.evil.com/login"),
            ("localhostevil.com", "https://localhostevil.com/"),
            ("127.0.0.1.evil.com", "https://127.0.0.1.evil.com/"),
            ("127.evil.com", "https://127.evil.com/"),
            ("localhost-login.com", "https://localhost-login.com/"),
        ] {
            assert_eq!(url_from_input(typed, &dir), Ok(u(expected)), "{typed}");
        }
        assert!(is_loopback_host("localhost") && is_loopback_host("127.0.0.1") && is_loopback_host("127.1.2.3"));
        assert!(!is_loopback_host("localhost.evil.com") && !is_loopback_host("127.evil.com") && !is_loopback_host("128.0.0.1"));
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

    // ---------- background loading ----------

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
        let fonts = Fonts::load();
        let mut seen = 0;
        for (_, SendPage(mut page)) in rx {
            seen += 1;
            assert_eq!((page.title.as_str(), page.images.len()), ("Threaded", 2));
            page.relayout(&fonts, 300.0); // lays out and clones the Rc'd images, on this thread
            assert!(page.layout.items.iter().any(|i| matches!(i, layout::Item::Image { .. })));
            assert_eq!(page.layout.controls.len(), 1, "form controls survive the handoff too");
        }
        assert_eq!(seen, 4);
    }
}
