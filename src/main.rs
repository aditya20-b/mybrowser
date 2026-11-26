mod chrome;
mod css;
mod dom;
mod fonts;
mod html;
mod images;
mod layout;
mod net;
mod page;
mod paint;
mod style;

use chrome::{AddressBar, ChromeView, Hit};
use fonts::Fonts;
use page::Page;
use paint::Painter;
use std::num::NonZeroU32;
use std::path::Path;
use std::rc::Rc;
use url::Url;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
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

/// Turn whatever the user typed (or passed on the command line) into a URL:
/// a full http/https/file/data URL, an existing file path, or a bare host such
/// as `example.com/page` (which gets `https://`, or `http://` for localhost).
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
    // A bare host: must look like one (a dot or localhost, an optional numeric
    // port, no credentials), so `mailto:a@b.c` is rejected instead of being
    // misread as https://b.c with a username.
    let authority = s.split(['/', '?', '#']).next().unwrap_or("");
    let port_ok = authority.rsplit_once(':').is_none_or(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    let looks_like_host = !authority.contains('@')
        && !authority.contains(' ')
        && (authority.contains('.') || authority.starts_with("localhost"))
        && port_ok;
    if looks_like_host {
        let local = authority.starts_with("localhost") || authority.starts_with("127.");
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

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(fonts, start, cwd);
    event_loop.run_app(&mut app).unwrap();
}

/// Headless mode: render the whole page to a PNG (handy for testing).
/// With `--chrome` semantics intentionally absent: this is the page only.
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

struct App {
    fonts: Fonts,
    painter: Painter,
    page: Page,
    bar: AddressBar,
    cwd: std::path::PathBuf,
    back: Vec<Url>,
    forward: Vec<Url>,
    scroll: f32,
    laid_out_width: f32,
    cursor: (f32, f32), // physical pixels
    hover: Option<String>,
    modifiers: ModifiersState,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    fn new(fonts: Fonts, start: Url, cwd: std::path::PathBuf) -> App {
        let page = Page::load(&start);
        let bar = AddressBar::new(page.url.as_str());
        App {
            fonts, painter: Painter::default(), page, bar, cwd, back: vec![], forward: vec![], scroll: 0.0,
            laid_out_width: -1.0, cursor: (0.0, 0.0), hover: None, modifiers: ModifiersState::empty(),
            window: None, surface: None,
        }
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
            let link = self.link_at(cx, cy - chrome::HEIGHT + self.scroll);
            (if link.is_some() { CursorIcon::Pointer } else { CursorIcon::Default }, link)
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

    /// Load `url` into the current tab. `record` pushes the page we're leaving
    /// onto the back stack (and drops the forward stack, as browsers do).
    fn open(&mut self, url: Url, record: bool) {
        if record {
            self.back.push(self.page.url.clone());
            self.forward.clear();
        }
        self.page = Page::load(&url);
        self.bar.show(self.page.url.as_str());
        self.laid_out_width = -1.0; // force relayout on the next redraw
        self.scroll = 0.0;
        self.update_hover();
        if let Some(w) = &self.window {
            w.set_title(&format!("{} — mybrowser", self.page.display_title()));
        }
        self.request_redraw();
    }

    fn reload(&mut self) {
        let (url, keep) = (self.page.url.clone(), self.scroll);
        self.open(url, false);
        self.scroll = keep; // clamped on the next redraw
    }

    fn go_back(&mut self) {
        if let Some(prev) = self.back.pop() {
            self.forward.push(self.page.url.clone());
            self.open(prev, false);
        }
    }

    fn go_forward(&mut self) {
        if let Some(next) = self.forward.pop() {
            self.back.push(self.page.url.clone());
            self.open(next, false);
        }
    }

    fn follow(&mut self, href: &str) {
        match resolve_link(&self.page.url, href) {
            Link::Nothing => {} // in-page anchors aren't scrolled to yet
            Link::External(url) => eprintln!("not supported (yet): {url}"),
            Link::Navigate(url) => self.open(url, true),
        }
    }

    fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let l = self.page.layout.links.iter().find(|l| x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h);
        l.map(|l| l.href.clone())
    }

    /// Enter pressed in the address bar.
    fn commit_address(&mut self) {
        match url_from_input(self.bar.text(), &self.cwd) {
            Ok(url) => self.open(url, true),
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
                let url = self.page.url.to_string();
                self.bar.show(&url);
            }
            self.update_hover(); // hit-test at release time rather than trusting a stale hover
            if let Some(href) = self.hover.clone() {
                self.follow(&href);
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
        let page = self.view_height() * 0.9;
        let alt_or_cmd = self.modifiers.alt_key() || self.modifiers.super_key();
        match &event.logical_key {
            Key::Character(c) if cmd => match c.as_str().to_ascii_lowercase().as_str() {
                "l" => self.bar.focus(),
                "r" => self.reload(),
                "[" => self.go_back(),
                "]" => self.go_forward(),
                _ => {}
            },
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
        let view = ChromeView { bar: &self.bar, can_back: !self.back.is_empty(), can_forward: !self.forward.is_empty() };
        self.painter.paint_chrome(pixels, w_px, h_px, &self.fonts, scale, &view);
        buffer.present().unwrap();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("{} — mybrowser", self.page.display_title()))
            .with_inner_size(LogicalSize::new(960.0, 720.0));
        let window = Rc::new(el.create_window(attrs).unwrap());
        let context = softbuffer::Context::new(window.clone()).unwrap();
        self.surface = Some(softbuffer::Surface::new(&context, window.clone()).unwrap());
        self.window = Some(window);
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
        assert_eq!(go("127.0.0.1:3000"), Ok(u("http://127.0.0.1:3000/")));
        assert_eq!(go("data:text/html,hi"), Ok(u("data:text/html,hi")));
        let file = go("page.html").unwrap();
        assert_eq!(file.scheme(), "file");
        assert!(file.path().ends_with("/page.html"));
        assert_eq!(go(file.as_str()), Ok(file));

        // things that are not addresses must not be navigated anywhere surprising
        for bad in ["", "   ", "javascript:alert(1)", "mailto:a@b.c", "user:pw@evil.com", "ftp://x.com/y", "nosuchfile", "two words.com", "host:notaport.com"] {
            assert!(go(bad).is_err(), "{bad:?} -> {:?}", go(bad));
        }
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
        // local pages can link to local files, with percent-encoding handled by the URL parser
        let local = u("file:///site/pages/index.html");
        assert_eq!(resolve_link(&local, "a%20b.html?x=1#top"), Link::Navigate(u("file:///site/pages/a%20b.html?x=1#top")));
        assert_eq!(resolve_link(&local, "file:///other.html"), Link::Navigate(u("file:///other.html")));
    }
}
