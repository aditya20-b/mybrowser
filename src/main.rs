mod bookmarks;
mod chrome;
mod cookies;
mod css;
mod dom;
mod download;
mod find;
mod fonts;
mod forms;
mod html;
mod images;
mod js;
mod jsparse;
mod memcount;

#[global_allocator]
static ALLOCATOR: memcount::Counting = memcount::Counting;
mod layout;
mod net;
mod page;
mod paint;
mod style;
mod tab;
mod tabbar;

use chrome::{AddressBar, ChromeView, Hit};
use dom::NodeId;
use find::Find;
use fonts::Fonts;
use forms::{ControlKind, FormState};
use page::Page;
use paint::Painter;
use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use tab::{is_loopback_host, Notify, Tab, UserEvent};
use tabbar::{StripView, TabHit};
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

const USAGE: &str = "usage: mybrowser [URL or file]\n       mybrowser --screenshot OUT.png [--width PX] [--scale N] [URL or file]";

/// The most text one paste into a page field may insert.
const MAX_PASTE_CHARS: usize = 100_000;

/// How many closed tabs Cmd+Shift+T can bring back.
const MAX_CLOSED_TABS: usize = 25;

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

/// What gets typed into a field when `clipboard` is pasted. Every kind of line break
/// (`\r\n`, a lone `\r`, `\n`) counts as one. A one-line field takes line breaks as
/// spaces, except those at the very start or end (a copied URL usually ends in one), and
/// drops other control characters; a textarea keeps its lines as `\n`. Capped, so pasting
/// a huge clipboard can't stall the page.
fn paste_text_for(kind: Option<ControlKind>, clipboard: &str) -> String {
    let multiline = kind == Some(ControlKind::Textarea);
    let normalized = clipboard.chars().take(MAX_PASTE_CHARS).collect::<String>().replace("\r\n", "\n").replace('\r', "\n");
    let text = if multiline { normalized.as_str() } else { normalized.trim_matches('\n') };
    text.chars()
        .filter_map(|c| match c {
            '\n' if multiline => Some('\n'),
            '\n' | '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

/// Remember a closed tab's address so Cmd+Shift+T can reopen it. Blank tabs aren't worth
/// reopening, and only the most recent `MAX_CLOSED_TABS` are kept.
fn remember_closed(closed: &mut Vec<Url>, url: &Url) {
    if url.scheme() == "about" {
        return;
    }
    closed.push(url.clone());
    if closed.len() > MAX_CLOSED_TABS {
        closed.remove(0);
    }
}

/// Turn whatever the user typed (or passed on the command line) into a URL:
/// a full http/https/file/data URL, an existing file path, or a bare host such
/// as `example.com/page` (which gets `https://`, or `http://` for loopback).
fn url_from_input(input: &str, cwd: &Path) -> Result<Url, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty address".into());
    }
    if s.eq_ignore_ascii_case("about:bookmarks") {
        return Ok(Url::parse("about:bookmarks").expect("a constant, valid URL"));
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
/// This is the page only, without the tab strip and toolbar.
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

/// Space above the page: the tab strip and the toolbar, in logical pixels.
const TOP: f32 = tabbar::HEIGHT + chrome::HEIGHT;

struct App {
    fonts: Fonts,
    painter: Painter,
    tabs: Vec<Tab>,
    active: usize,
    next_tab_id: u64,
    bar: AddressBar,
    /// Find-in-page, while its box is open (it belongs to the active tab's page).
    find: Option<Find>,
    /// Addresses of tabs that were closed, most recent last.
    closed: Vec<Url>,
    cwd: std::path::PathBuf,
    notify: Notify,
    cursor: (f32, f32), // physical pixels
    hover: Option<String>,
    modifiers: ModifiersState,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    fn new(fonts: Fonts, start: Url, cwd: std::path::PathBuf, proxy: EventLoopProxy<UserEvent>) -> App {
        let proxy = Mutex::new(proxy);
        let notify: Notify = Arc::new(move |event| {
            if let Ok(p) = proxy.lock() {
                let _ = p.send_event(event);
            }
        });
        let first = Tab::new(0, start, None, notify.clone());
        let bar = AddressBar::new(&first.address());
        App {
            fonts, painter: Painter::default(), tabs: vec![first], active: 0, next_tab_id: 1, bar, find: None, closed: vec![], cwd, notify,
            cursor: (0.0, 0.0), hover: None, modifiers: ModifiersState::empty(), window: None, surface: None,
        }
    }

    fn tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
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

    /// Height of the area the page is drawn in (the window minus the tab strip and toolbar).
    /// (in page units: a zoomed-in page shows less of itself)
    fn view_height(&self) -> f32 {
        (self.window_size().1 - TOP).max(0.0) / self.tab().zoom
    }

    /// Cmd+Plus, Cmd+Minus, Cmd+0: zoom the page in this tab.
    /// (the window title shows the level when it isn't 100%; the next redraw clamps the scroll)
    fn zoom_by(&mut self, direction: i32) {
        self.tab_mut().zoom_step(direction);
        self.set_title();
        self.request_redraw();
    }

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn set_title(&self) {
        if let Some(w) = &self.window {
            let tab = self.tab();
            let title = if tab.is_loading() { "Loading…".to_string() } else { tab.title() };
            let zoom = if (tab.zoom - 1.0).abs() > 0.001 { format!(" ({}%)", (tab.zoom * 100.0).round()) } else { String::new() };
            w.set_title(&format!("{title} — mybrowser{zoom}"));
        }
    }

    /// Show the active tab's address and title (after anything that navigates or
    /// switches tabs). Leaves address editing, like a navigation always has.
    fn sync_chrome(&mut self) {
        let address = self.tab().address();
        self.bar.show(&address);
        self.set_title();
        self.request_redraw();
    }

    fn clamp_scroll(&mut self) {
        let view = self.view_height();
        let tab = self.tab_mut();
        let max = (tab.page.layout.height - view).max(0.0);
        tab.scroll = tab.scroll.clamp(0.0, max);
    }

    /// Recompute what is under the mouse and set the cursor icon. Must run
    /// whenever the page moves under a stationary cursor: scrolling, relayout,
    /// navigation, switching tabs.
    fn update_hover(&mut self) {
        let s = self.scale();
        let (cx, cy) = (self.cursor.0 / s, self.cursor.1 / s);
        let (icon, hover) = if cy < tabbar::HEIGHT {
            let hit = tabbar::hit_test(cx, cy, self.tabs.len(), self.window_size().0);
            (if hit.is_some() { CursorIcon::Pointer } else { CursorIcon::Default }, None)
        } else if cy < TOP {
            let icon = match chrome::hit_test(cx, cy - tabbar::HEIGHT, self.window_size().0) {
                Some(Hit::Address) => CursorIcon::Text,
                Some(_) => CursorIcon::Pointer,
                None => CursorIcon::Default,
            };
            (icon, None)
        } else {
            let zoom = self.tab().zoom;
            let (px, py) = (cx / zoom, (cy - TOP) / zoom + self.tab().scroll);
            let tab = self.tab();
            if let Some(id) = tab.page.control_at(px, py) {
                // an I-beam over text fields, a hand over buttons, boxes and menus
                let text = forms::kind_of(&tab.page.dom, id).is_some_and(forms::is_text_like);
                (if text { CursorIcon::Text } else { CursorIcon::Pointer }, None)
            } else {
                let link = tab.link_at(px, py);
                (if link.is_some() { CursorIcon::Pointer } else { CursorIcon::Default }, link)
            }
        };
        self.hover = hover;
        if let Some(w) = &self.window {
            w.set_cursor(icon);
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.tab_mut().scroll += dy;
        self.clamp_scroll();
        self.update_hover();
        self.request_redraw();
    }

    // ---------- find in page ----------

    /// Open the find box (Cmd+F), taking keyboard focus from the address bar
    /// and from any page field (typing would otherwise land in the query).
    fn open_find(&mut self) {
        if self.find.is_none() {
            self.find = Some(Find::new());
        }
        let address = self.tab().address();
        self.bar.show(&address); // leave address editing
        let tab = self.tab_mut();
        if tab.page.form.focus.take().is_some() {
            tab.form_changed();
        }
        self.request_redraw();
    }

    fn close_find(&mut self) {
        if self.find.take().is_some() {
            self.request_redraw();
        }
    }

    /// Scroll so the selected match is in view.
    fn scroll_to_find(&mut self) {
        let view = self.view_height();
        if let Some(target) = self.find.as_ref().and_then(|f| f.scroll_for_current(view)) {
            self.tab_mut().scroll = target;
            self.clamp_scroll();
            self.update_hover();
        }
        self.request_redraw();
    }

    /// The query changed: search again from the first match and show it.
    fn find_query_changed(&mut self) {
        if let Some(find) = self.find.as_mut() {
            find.search(&self.tabs[self.active].page.layout, &self.fonts);
        }
        self.scroll_to_find();
    }

    fn find_step(&mut self, forward: bool) {
        if let Some(find) = self.find.as_mut() {
            if forward { find.next() } else { find.prev() }
        }
        self.scroll_to_find();
    }

    /// A key while the find box is open. Returns true if it was used.
    fn find_key(&mut self, event: &KeyEvent, cmd: bool) -> bool {
        if self.find.is_none() {
            return false;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.close_find();
                true
            }
            Key::Named(NamedKey::Enter) => {
                let forward = !self.modifiers.shift_key();
                self.find_step(forward);
                true
            }
            Key::Named(NamedKey::Backspace) if !cmd => {
                if let Some(f) = self.find.as_mut() {
                    f.backspace();
                }
                self.find_query_changed();
                true
            }
            Key::Character(c) if cmd && c.eq_ignore_ascii_case("v") => {
                if let (Some(text), Some(f)) = (chrome::clipboard_get(), self.find.as_mut()) {
                    f.type_text(&text);
                }
                self.find_query_changed();
                true
            }
            _ if !cmd => match &event.text {
                // (Tab and Delete may come with control characters as their text: not typing)
                Some(text) if !text.is_empty() && !text.chars().any(|c| c.is_control()) => {
                    if let Some(f) = self.find.as_mut() {
                        f.type_text(text.as_str());
                    }
                    self.find_query_changed();
                    true
                }
                _ => false, // arrows, Page Down and the like still scroll the page
            },
            _ => false,
        }
    }

    // ---------- tabs ----------

    /// Open a tab (loading `url`, or empty) and switch to it.
    fn new_tab(&mut self, url: Option<Url>, initiator: Option<Url>) {
        if self.tabs.len() >= tabbar::MAX_TABS {
            self.bar.error = Some(format!("too many tabs open (the limit is {})", tabbar::MAX_TABS));
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let blank = url.is_none();
        let tab = match url {
            Some(u) => Tab::new(id, u, initiator, self.notify.clone()),
            None => Tab::blank(id, self.notify.clone()),
        };
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.find = None;
        self.sync_chrome();
        if blank {
            self.bar.focus(); // ready to type an address
        }
        self.update_hover();
    }

    /// Cmd+Shift+T: bring back the most recently closed tab.
    fn reopen_closed_tab(&mut self) {
        if self.tabs.len() >= tabbar::MAX_TABS {
            self.bar.error = Some(format!("too many tabs open (the limit is {})", tabbar::MAX_TABS));
            return;
        }
        if let Some(url) = self.closed.pop() {
            self.new_tab(Some(url), None);
            // (a reopened tab is not a fresh request: if its address is a file, nothing is saved)
            self.tab_mut().mark_restored();
        }
    }

    /// Close tab `index`. The last tab is replaced by an empty one, not removed.
    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        // what the user was looking at (or heading for, if the tab never got there)
        let address = self.tabs[index].nav.pending().map_or_else(|| self.tabs[index].page.url.clone(), |p| p.url.clone());
        remember_closed(&mut self.closed, &address);
        let shown_before = self.active;
        if self.tabs.len() == 1 {
            let id = self.next_tab_id;
            self.next_tab_id += 1;
            self.tabs[0] = Tab::blank(id, self.notify.clone());
            self.active = 0;
        } else {
            self.tabs.remove(index);
            self.active = tabbar::active_after_close(self.active, index, self.tabs.len());
        }
        // closing a background tab leaves the page you are searching alone
        if index == shown_before {
            self.find = None;
        }
        self.sync_chrome();
        self.update_hover();
        self.show_download_notice();
    }

    /// Show what happened to a download that finished while the active tab was in the
    /// background (the note waits in its tab until it is looked at).
    fn show_download_notice(&mut self) {
        if let Some(note) = self.tabs[self.active].take_notice() {
            self.bar.error = Some(note);
        }
    }

    fn switch_tab(&mut self, index: usize) {
        if index < self.tabs.len() && index != self.active {
            self.active = index;
            self.find = None;
            self.sync_chrome();
            self.update_hover();
            self.show_download_notice();
        }
    }

    // ---------- navigation in the active tab ----------

    fn open(&mut self, url: Url) {
        self.tab_mut().open(url);
        self.sync_chrome();
    }

    fn reload(&mut self) {
        self.tab_mut().reload();
        self.sync_chrome();
    }

    /// Cmd+D: bookmark the page on screen, or remove its bookmark if it has one.
    fn toggle_bookmark(&mut self) {
        // not while a load is in flight (the address bar already shows the next page, the one
        // on screen is about to go), and not a page that failed to load
        if self.tab().is_loading() {
            self.bar.error = Some("wait for the page to finish loading before bookmarking it".into());
            return;
        }
        if self.tab().page.is_error || self.tab().page.status.is_some_and(|s| s >= 400) {
            self.bar.error = Some("a page that failed to load can't be bookmarked".into());
            return;
        }
        let url = self.tab().page.url.clone();
        let title = download::display_safe(&self.tab().page.display_title(), 120);
        self.bar.error = Some(match bookmarks::data_dir() {
            None => "no folder to keep bookmarks in".to_string(),
            Some(dir) => match bookmarks::toggle(&dir, &url, &title) {
                Ok(true) => format!("Bookmarked: {title}"),
                Ok(false) => "Bookmark removed".to_string(),
                Err(e) => e,
            },
        });
    }

    fn go_back(&mut self) {
        let swapped = self.tab_mut().go_back();
        self.after_history_move(swapped);
    }

    fn go_forward(&mut self) {
        let swapped = self.tab_mut().go_forward();
        self.after_history_move(swapped);
    }

    /// A Back or Forward may have swapped in a remembered page on the spot (no load to wait
    /// for), so a find in progress was searching the page that just went away.
    fn after_history_move(&mut self, swapped: bool) {
        if swapped {
            self.find = None;
            self.update_hover();
        }
        self.sync_chrome();
    }

    fn cancel_load(&mut self) {
        if self.tab_mut().cancel_load() {
            self.sync_chrome();
        }
    }

    /// A loader thread delivered a page for some tab (which may have been closed since).
    fn user_event_loaded(&mut self, tab: u64, ticket: u64, page: Page) {
        let Some(index) = self.tabs.iter().position(|t| t.id == tab) else { return };
        if !self.tabs[index].finish_load(ticket, page) {
            return; // superseded or cancelled
        }
        if index == self.active {
            self.find = None; // it was searching the old page
            self.sync_chrome();
            self.update_hover();
            // a download's outcome replaces the address text until the next navigation
            if let Some(note) = self.tabs[index].take_notice() {
                self.bar.error = Some(note);
            }
        } else {
            self.request_redraw(); // its label changed
        }
    }

    // ---------- forms ----------

    /// The result of a click or key that may have submitted a form.
    fn after_form_action(&mut self, result: Result<(), String>) {
        match result {
            Ok(()) => self.sync_chrome(),
            Err(why) => self.bar.error = Some(why),
        }
    }

    /// A click landed on control `id`.
    fn activate_control(&mut self, id: NodeId) {
        let result = self.tab_mut().activate_control(id);
        self.after_form_action(result);
    }

    fn submit_form(&mut self, control: NodeId, submitter: Option<NodeId>) {
        let result = self.tab_mut().submit_form(control, submitter);
        self.after_form_action(result);
    }

    /// Cmd+V / Cmd+C in a focused page field. Returns true if the key was handled
    /// (a field that isn't text-like leaves Cmd shortcuts to the browser).
    fn field_clipboard_key(&mut self, id: NodeId, kind: Option<ControlKind>, key: &Key) -> bool {
        if !kind.is_some_and(forms::is_text_like) {
            return false;
        }
        // Cmd+Left / Cmd+Right jump to the start / end of the text (they'd otherwise mean Back / Forward)
        if let Key::Named(named @ (NamedKey::ArrowLeft | NamedKey::ArrowRight)) = key {
            let how = if *named == NamedKey::ArrowLeft { forms::CaretMove::Home } else { forms::CaretMove::End };
            let tab = self.tab_mut();
            let Page { dom, form, .. } = &mut tab.page;
            form.move_caret(dom, id, how);
            tab.form_changed();
            return true;
        }
        let Key::Character(c) = key else { return false };
        match c.as_str().to_ascii_lowercase().as_str() {
            "v" => {
                if let Some(clip) = chrome::clipboard_get() {
                    let text = paste_text_for(kind, &clip);
                    if !text.is_empty() {
                        let tab = self.tab_mut();
                        tab.page.dom_value_edit(id, &text);
                        tab.form_changed();
                    }
                }
                true
            }
            "c" => {
                // a password never goes to the clipboard
                if kind != Some(ControlKind::Password) {
                    let tab = self.tab();
                    chrome::clipboard_set(&tab.page.form.value(&tab.page.dom, id));
                }
                true
            }
            _ => false,
        }
    }

    /// A key press while a form control has focus. Returns true if the control
    /// consumed it. Typing in a text field consumes everything (so `r` types an
    /// `r` instead of reloading); other controls only take Space, Enter, Tab and Escape.
    fn control_key(&mut self, event: &KeyEvent, cmd: bool) -> bool {
        let Some(id) = self.tab().page.form.focus else { return false };
        let kind = forms::kind_of(&self.tab().page.dom, id);
        if cmd {
            // Cmd+V pastes into the field and Cmd+C copies from it; Cmd+L, Cmd+R ... still work
            return self.field_clipboard_key(id, kind, &event.logical_key);
        }
        let text_like = kind.is_some_and(forms::is_text_like);
        let shift = self.modifiers.shift_key();
        // Holding Enter or Space must not toggle a box or navigate over and over.
        if event.repeat
            && matches!(event.logical_key, Key::Named(NamedKey::Enter | NamedKey::Space))
            && !matches!(kind, Some(ControlKind::Textarea))
            && !matches!(event.logical_key, Key::Named(NamedKey::Space) if text_like)
        {
            return true;
        }
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                let tab = self.tab_mut();
                tab.page.form.focus = None;
                tab.form_changed();
                true
            }
            Key::Named(NamedKey::Tab) => {
                let tab = self.tab_mut();
                let Page { dom, form, .. } = &mut tab.page;
                form.focus_next(dom, !shift);
                tab.form_changed();
                true
            }
            Key::Named(NamedKey::Enter) => {
                match kind {
                    Some(ControlKind::Textarea) => self.tab_mut().page.dom_value_edit(id, "\n"),
                    Some(ControlKind::Submit) => self.submit_form(id, Some(id)),
                    Some(ControlKind::Text | ControlKind::Password) => {
                        // implicit submission: Enter "clicks" the form's first submit button
                        if let Some(form_el) = forms::form_of(&self.tab().page.dom, id) {
                            let button = FormState::default_button(&self.tab().page.dom, form_el);
                            self.submit_form(id, button);
                        }
                    }
                    Some(ControlKind::Checkbox | ControlKind::Radio | ControlKind::Select | ControlKind::Button) => self.activate_control(id),
                    _ => {}
                }
                self.tab_mut().form_changed();
                true
            }
            Key::Named(key @ (NamedKey::ArrowUp | NamedKey::ArrowDown | NamedKey::Home | NamedKey::End)) if kind == Some(ControlKind::Select) => {
                let tab = self.tab_mut();
                let Page { dom, form, .. } = &mut tab.page;
                let step = match key {
                    NamedKey::ArrowUp => forms::SelectStep::Prev,
                    NamedKey::ArrowDown => forms::SelectStep::Next,
                    NamedKey::Home => forms::SelectStep::First,
                    _ => forms::SelectStep::Last,
                };
                form.step_select(dom, id, step);
                tab.form_changed();
                true
            }
            Key::Named(key @ (NamedKey::ArrowLeft | NamedKey::ArrowRight | NamedKey::Home | NamedKey::End | NamedKey::Delete)) if text_like => {
                let tab = self.tab_mut();
                let Page { dom, form, .. } = &mut tab.page;
                match key {
                    NamedKey::ArrowLeft => form.move_caret(dom, id, forms::CaretMove::Left),
                    NamedKey::ArrowRight => form.move_caret(dom, id, forms::CaretMove::Right),
                    NamedKey::Home => form.move_caret(dom, id, forms::CaretMove::Home),
                    NamedKey::End => form.move_caret(dom, id, forms::CaretMove::End),
                    _ => form.delete_forward(dom, id),
                }
                tab.form_changed();
                true
            }
            Key::Named(NamedKey::Backspace) if text_like => {
                let tab = self.tab_mut();
                let Page { dom, form, .. } = &mut tab.page;
                form.backspace(dom, id);
                tab.form_changed();
                true
            }
            Key::Named(NamedKey::Space)
                if matches!(kind, Some(ControlKind::Checkbox | ControlKind::Radio | ControlKind::Select | ControlKind::Submit | ControlKind::Button)) =>
            {
                self.activate_control(id);
                true
            }
            _ if text_like => {
                let tab = self.tab_mut();
                if let Some(text) = &event.text {
                    tab.page.dom_value_edit(id, text.as_str());
                }
                tab.form_changed();
                true
            }
            _ => false,
        }
    }

    /// Move keyboard focus to the address bar. A page field can't keep focus at
    /// the same time (typing would otherwise go back to it once the bar is left).
    fn focus_address_bar(&mut self) {
        let tab = self.tab_mut();
        if tab.page.form.focus.take().is_some() {
            tab.form_changed();
        }
        self.bar.focus();
    }

    /// Enter pressed in the address bar. Going to the page you're already on is
    /// a reload, not a new history entry.
    fn commit_address(&mut self) {
        let typed = self.bar.submit_text().to_string();
        match url_from_input(&typed, &self.cwd) {
            Ok(url) if url == self.tab().page.url && !self.tab().is_loading() => self.reload(),
            Ok(url) => self.open(url),
            Err(e) => self.bar.error = Some(e),
        }
    }

    /// Middle button released: on a tab it closes that tab.
    fn middle_click(&mut self) {
        let s = self.scale();
        let (cx, cy) = (self.cursor.0 / s, self.cursor.1 / s);
        if cy < tabbar::HEIGHT {
            if let Some(TabHit::Select(i) | TabHit::Close(i)) = tabbar::hit_test(cx, cy, self.tabs.len(), self.window_size().0) {
                self.close_tab(i);
            }
        }
        self.request_redraw();
    }

    /// Left click released at the current cursor position.
    fn click(&mut self) {
        let s = self.scale();
        let (cx, cy) = (self.cursor.0 / s, self.cursor.1 / s);
        let width = self.window_size().0;
        if cy < tabbar::HEIGHT {
            match tabbar::hit_test(cx, cy, self.tabs.len(), width) {
                Some(TabHit::Select(i)) => self.switch_tab(i),
                Some(TabHit::Close(i)) => self.close_tab(i),
                Some(TabHit::New) => self.new_tab(None, None),
                None => {}
            }
        } else if cy < TOP {
            match chrome::hit_test(cx, cy - tabbar::HEIGHT, width) {
                Some(Hit::Back) => self.go_back(),
                Some(Hit::Forward) => self.go_forward(),
                Some(Hit::Reload) => self.reload(),
                Some(Hit::Address) => {
                    if self.bar.is_focused() {
                        self.bar.click_again();
                    } else {
                        self.focus_address_bar();
                    }
                }
                None => {}
            }
        } else {
            // clicking the page ends address editing (a blank tab shows an empty address, not about:blank)
            if self.bar.is_focused() {
                let url = if self.tab().is_loading() { self.bar.submit_text().to_string() } else { self.tab().address() };
                self.bar.show(&url);
            }
            let zoom = self.tab().zoom;
            let (px, py) = (cx / zoom, (cy - TOP) / zoom + self.tab().scroll);
            if let Some(id) = self.tab().page.control_at(px, py) {
                // clicking a page field takes the keyboard from the find box
                self.close_find();
                if !self.tab_mut().script_click(id) {
                    self.activate_control(id);
                }
                self.sync_chrome(); // (a handler may have sent the tab somewhere)
            } else {
                // clicking anywhere else leaves the control that had focus
                let tab = self.tab_mut();
                if tab.page.form.focus.take().is_some() {
                    tab.form_changed();
                }
                self.update_hover(); // hit-test at release time rather than trusting a stale hover
                // (a click on anything else is still a click on the page: it starts at `<body>`, so handlers
                // on the body and the document hear it; which element was under the cursor isn't tracked)
                let clicked = self
                    .tab()
                    .link_node_at(px, py)
                    .or_else(|| self.tab().page.element_at(px, py))
                    .or_else(|| self.tab().page.body_node());
                // a script's click handler runs first; `preventDefault()` keeps the link from being followed
                let prevented = clicked.is_some_and(|n| self.tab_mut().script_click(n));
                self.sync_chrome();
                if let (Some(href), false) = (self.hover.clone(), prevented) {
                    if self.modifiers.super_key() {
                        // Cmd+click opens the link in a new tab
                        if let Some(url) = self.tab().link_target(&href) {
                            let from = self.tab().page.url.clone();
                            self.new_tab(Some(url), Some(from));
                        }
                    } else if self.tab_mut().follow(&href) {
                        self.sync_chrome();
                    } else {
                        // a link to a `#section` of this page scrolls (and the find highlight stays valid)
                        self.clamp_scroll();
                        self.update_hover();
                        self.sync_chrome(); // the address now names the fragment
                    }
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
                self.tab_mut().cancel_load();
                self.sync_chrome(); // shows the page's own address again
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

    /// Shortcuts that work wherever the focus is: new tab, close tab, switch tab, find.
    /// Returns true if the key was one of them.
    fn tab_shortcut(&mut self, event: &KeyEvent, cmd: bool) -> bool {
        match &event.logical_key {
            Key::Named(NamedKey::Tab) if self.modifiers.control_key() => {
                let forward = !self.modifiers.shift_key();
                let next = tabbar::cycle(self.active, self.tabs.len(), forward);
                self.switch_tab(next);
                true
            }
            Key::Character(c) if cmd => match c.as_str().to_ascii_lowercase().as_str() {
                // zoom works wherever the focus is (also while typing an address)
                "=" | "+" => {
                    self.zoom_by(1);
                    true
                }
                "-" => {
                    self.zoom_by(-1);
                    true
                }
                "0" => {
                    self.zoom_by(0);
                    true
                }
                "t" | "w" if event.repeat => true, // holding the key must not open or close dozens of tabs
                "t" if self.modifiers.shift_key() => {
                    self.reopen_closed_tab();
                    true
                }
                "t" => {
                    self.new_tab(None, None);
                    true
                }
                "w" => {
                    self.close_tab(self.active);
                    true
                }
                "f" => {
                    self.open_find();
                    true
                }
                "g" if self.find.is_some() => {
                    let forward = !self.modifiers.shift_key();
                    self.find_step(forward);
                    true
                }
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && d != "0" => {
                    // Cmd+1..8 select that tab, Cmd+9 the last one
                    let n = (d.as_bytes()[0] - b'0') as usize;
                    self.switch_tab(if n == 9 { self.tabs.len() - 1 } else { n - 1 });
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn on_key(&mut self, event: &KeyEvent) {
        let cmd = self.modifiers.super_key() || self.modifiers.control_key();
        if self.tab_shortcut(event, cmd) {
            self.request_redraw();
            return;
        }
        if self.bar.is_focused() {
            self.address_key(event, cmd);
            self.request_redraw();
            return;
        }
        if self.find_key(event, cmd) {
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
                "l" => self.focus_address_bar(),
                "d" => self.toggle_bookmark(),
                "b" if self.modifiers.shift_key() => self.open(Url::parse("about:bookmarks").expect("a constant, valid URL")),
                "r" => self.reload(),
                "[" => self.go_back(),
                "]" => self.go_forward(),
                "." => self.cancel_load(),
                _ => {}
            },
            Key::Named(NamedKey::Escape) => self.cancel_load(),
            Key::Named(NamedKey::Tab) => {
                // Tab into the page's first control (Shift+Tab: the last)
                let shift = self.modifiers.shift_key();
                let tab = self.tab_mut();
                let Page { dom, form, .. } = &mut tab.page;
                form.focus_next(dom, !shift);
                tab.form_changed();
            }
            Key::Named(NamedKey::ArrowDown) => self.scroll_by(48.0 / self.tab().zoom),
            Key::Named(NamedKey::ArrowUp) => self.scroll_by(-48.0 / self.tab().zoom),
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
        let zoom = self.tabs[self.active].zoom;
        // the page is laid out `zoom` times narrower and drawn `zoom` times bigger; the toolbar and tabs keep their size
        let logical_w = size.width as f32 / scale / zoom;
        let relaid = {
            let tab = &mut self.tabs[self.active];
            let needed = (logical_w - tab.laid_out_width).abs() > 0.5;
            if needed {
                tab.page.relayout(&self.fonts, logical_w);
                tab.laid_out_width = logical_w;
                // a page opened at `page.html#section` scrolls to it once there is a layout
                tab.settle_fragment();
            }
            needed
        };
        // a different layout means the matches moved
        if relaid {
            if let Some(find) = self.find.as_mut() {
                find.refresh(&self.tabs[self.active].page.layout, &self.fonts);
            }
        }
        // The page height or the window height may have changed (resize, reload,
        // navigation), so the clamp and the hover state are refreshed every frame.
        self.clamp_scroll();
        self.update_hover();

        let (w_px, h_px) = (size.width as usize, size.height as usize);
        let tab_px = ((tabbar::HEIGHT * scale).round() as usize).min(h_px);
        let bar_px = (chrome::HEIGHT * scale).round() as usize;
        let top = (tab_px + bar_px).min(h_px);
        let titles: Vec<String> = self.tabs.iter().map(|t| t.title()).collect();
        let loading: Vec<bool> = self.tabs.iter().map(|t| t.is_loading()).collect();
        let tab = &self.tabs[self.active];
        let surface = self.surface.as_mut().unwrap();
        surface.resize(w, h).unwrap();
        let mut buffer = surface.buffer_mut().unwrap();
        let pixels: &mut [u32] = &mut buffer;
        // The page goes in the rows below the chrome (with the find overlay on top of it);
        // then the toolbar, then the tab strip.
        self.painter.paint(&mut pixels[top * w_px..], w_px, h_px - top, &tab.page.layout, &self.fonts, tab.scroll, scale * zoom);
        if let Some(find) = &self.find {
            self.painter.paint_find(&mut pixels[top * w_px..], w_px, h_px - top, &self.fonts, scale, zoom, tab.scroll, find);
        }
        let view = ChromeView {
            bar: &self.bar,
            can_back: tab.history.back_target().is_some(),
            can_forward: tab.history.forward_target().is_some(),
        };
        self.painter.paint_chrome(&mut pixels[tab_px * w_px..], w_px, h_px - tab_px, &self.fonts, scale, &view);
        let strip = StripView { titles: &titles, loading: &loading, active: self.active };
        self.painter.paint_tabs(&mut pixels[..tab_px * w_px], w_px, tab_px, &self.fonts, scale, &strip);
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
            UserEvent::Loaded { tab, ticket, page } => self.user_event_loaded(tab, ticket, page.0),
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
                // (window pixels moved, in page units)
                let zoom = self.tab().zoom;
                self.scroll_by(dy / zoom);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
                self.update_hover();
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => self.click(),
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Middle, .. } => self.middle_click(),
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
    fn closed_tabs_are_remembered_most_recent_last_and_blank_ones_are_not() {
        let mut closed = vec![];
        remember_closed(&mut closed, &u("about:blank"));
        assert!(closed.is_empty(), "an empty tab isn't worth reopening");
        remember_closed(&mut closed, &u("https://a.test/"));
        remember_closed(&mut closed, &u("https://b.test/x?y=1#z"));
        assert_eq!(closed.pop(), Some(u("https://b.test/x?y=1#z")), "the newest comes back first");
        assert_eq!(closed.pop(), Some(u("https://a.test/")));
        assert_eq!(closed.pop(), None);
        // the list can't grow without bound: the oldest are forgotten
        for i in 0..MAX_CLOSED_TABS + 10 {
            remember_closed(&mut closed, &u(&format!("https://site{i}.test/")));
        }
        assert_eq!(closed.len(), MAX_CLOSED_TABS);
        assert_eq!(closed.last(), Some(&u(&format!("https://site{}.test/", MAX_CLOSED_TABS + 9))));
        assert_eq!(closed.first(), Some(&u("https://site10.test/")));
    }

    #[test]
    fn pasted_text_fits_the_field_it_goes_into() {
        // a one-line field turns line breaks into spaces and drops control characters
        assert_eq!(paste_text_for(Some(ControlKind::Text), "a\r\nb\nc\td\u{7}e"), "a b c de");
        assert_eq!(paste_text_for(Some(ControlKind::Password), "pass\nword"), "pass word");
        // a textarea keeps its lines (and loses the carriage returns)
        assert_eq!(paste_text_for(Some(ControlKind::Textarea), "one\r\ntwo\nthree"), "one\ntwo\nthree");
        // escape sequences and other controls never reach a field
        assert_eq!(paste_text_for(Some(ControlKind::Textarea), "\u{1b}[31mred\u{0}"), "[31mred");
        // unicode survives, and a huge clipboard is cut off
        assert_eq!(paste_text_for(Some(ControlKind::Text), "é日本語🎉"), "é日本語🎉");
        assert_eq!(paste_text_for(Some(ControlKind::Text), &"x".repeat(MAX_PASTE_CHARS * 3)).chars().count(), MAX_PASTE_CHARS);
        assert_eq!(paste_text_for(None, ""), "");
    }

    #[test]
    fn a_lone_carriage_return_is_a_line_break_and_edge_breaks_are_trimmed() {
        // classic Mac line endings used to vanish, gluing the words together
        assert_eq!(paste_text_for(Some(ControlKind::Text), "a\rb"), "a b");
        assert_eq!(paste_text_for(Some(ControlKind::Textarea), "a\rb\r\nc"), "a\nb\nc");
        // a copied URL ends in a newline: it must not become a trailing space in a one-line field
        assert_eq!(paste_text_for(Some(ControlKind::Text), "https://x.test\n"), "https://x.test");
        assert_eq!(paste_text_for(Some(ControlKind::Text), "\r\n\nabc\n\r\n"), "abc");
        assert_eq!(paste_text_for(Some(ControlKind::Password), "secret\r\n"), "secret");
        // but breaks in the middle still become spaces, and a textarea keeps all of its own
        assert_eq!(paste_text_for(Some(ControlKind::Text), "\nfirst\n\nsecond\n"), "first  second");
        assert_eq!(paste_text_for(Some(ControlKind::Textarea), "\nabc\n"), "\nabc\n");
        assert_eq!(paste_text_for(Some(ControlKind::Text), "\n\n"), "");
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
    }
}
