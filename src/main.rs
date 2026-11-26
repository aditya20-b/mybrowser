mod css;
mod dom;
mod fonts;
mod html;
mod images;
mod layout;
mod page;
mod paint;
mod style;

use fonts::Fonts;
use page::Page;
use paint::Painter;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

const USAGE: &str = "usage: mybrowser [page.html]\n       mybrowser --screenshot OUT.png [--width PX] [--scale N] [page.html]";

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
    let path = PathBuf::from(args.first().map(|s| s.as_str()).unwrap_or("pages/index.html"));

    let fonts = Fonts::load();
    if let Some(out) = screenshot {
        screenshot_to_png(&path, &fonts, width, scale, Path::new(&out));
        return;
    }

    let event_loop = EventLoop::new().unwrap();
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App::new(fonts, path);
    event_loop.run_app(&mut app).unwrap();
}

/// Headless mode: render the whole page to a PNG (handy for testing).
fn screenshot_to_png(path: &Path, fonts: &Fonts, width: f32, scale: f32, out: &Path) {
    let t0 = std::time::Instant::now();
    let mut page = Page::load(path);
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
    eprintln!("parse+style {t_load:?}, layout {t_layout:?}, paint {t_paint:?}");
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
    /// Nothing to do: empty href or an in-page `#anchor`.
    Nothing,
    /// A scheme (`http:`, `mailto:`, `javascript:` ...) or `//host/...`: not supported.
    External(String),
    File(PathBuf),
}

fn resolve_link(base_dir: &Path, href: &str) -> Link {
    let target = href.split(['#', '?']).next().unwrap_or("").trim();
    if target.is_empty() {
        return Link::Nothing;
    }
    // A colon in the first path segment means a URL scheme, as in browsers.
    let has_scheme = target.split('/').next().is_some_and(|seg| seg.contains(':'));
    if has_scheme || target.starts_with("//") {
        return Link::External(href.to_string());
    }
    Link::File(base_dir.join(page::percent_decode(target)))
}

struct App {
    fonts: Fonts,
    painter: Painter,
    page: Page,
    history: Vec<PathBuf>,
    scroll: f32,
    laid_out_width: f32,
    cursor: (f32, f32), // physical pixels
    hover: Option<String>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    fn new(fonts: Fonts, path: PathBuf) -> App {
        let page = Page::load(&path);
        App {
            fonts, painter: Painter::default(), page, history: vec![], scroll: 0.0,
            laid_out_width: -1.0, cursor: (0.0, 0.0), hover: None, window: None, surface: None,
        }
    }

    fn scale(&self) -> f32 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32)
    }

    fn view_height(&self) -> f32 {
        self.window.as_ref().map_or(0.0, |w| w.inner_size().height as f32) / self.scale()
    }

    fn clamp_scroll(&mut self) {
        let max = (self.page.layout.height - self.view_height()).max(0.0);
        self.scroll = self.scroll.clamp(0.0, max);
    }

    /// Recompute which link is under the mouse. Must run whenever the page moves
    /// under a stationary cursor: scrolling, relayout, navigation.
    fn update_hover(&mut self) {
        let s = self.scale();
        self.hover = self.link_at(self.cursor.0 / s, self.cursor.1 / s + self.scroll);
        if let Some(w) = &self.window {
            w.set_cursor(if self.hover.is_some() { CursorIcon::Pointer } else { CursorIcon::Default });
        }
    }

    fn scroll_by(&mut self, dy: f32) {
        self.scroll += dy;
        self.clamp_scroll();
        self.update_hover();
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn open(&mut self, path: PathBuf, push_history: bool) {
        if push_history {
            self.history.push(self.page.path.clone());
        }
        self.page = Page::load(&path);
        self.laid_out_width = -1.0; // force relayout on the next redraw
        self.scroll = 0.0;
        self.update_hover();
        if let Some(w) = &self.window {
            w.set_title(&format!("{} — mybrowser", if self.page.title.is_empty() { "untitled" } else { &self.page.title }));
            w.request_redraw();
        }
    }

    fn follow(&mut self, href: &str) {
        let dir = self.page.path.parent().unwrap_or(Path::new(".")).to_path_buf();
        match resolve_link(&dir, href) {
            Link::Nothing => {} // in-page anchors aren't supported yet
            Link::External(url) => eprintln!("only local files are supported so far: {url}"),
            Link::File(path) => {
                if path == self.page.path {
                    return; // already here: don't push a duplicate history entry
                }
                if !path.is_file() {
                    eprintln!("no such file: {}", path.display());
                    return; // keep the current page instead of replacing it with an error page
                }
                self.open(path, true);
            }
        }
    }

    fn link_at(&self, x: f32, y: f32) -> Option<String> {
        let l = self.page.layout.links.iter().find(|l| x >= l.x && x <= l.x + l.w && y >= l.y && y <= l.y + l.h);
        l.map(|l| l.href.clone())
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
        let surface = self.surface.as_mut().unwrap();
        surface.resize(w, h).unwrap();
        let mut buffer = surface.buffer_mut().unwrap();
        self.painter.paint(&mut buffer, size.width as usize, size.height as usize,
            &self.page.layout, &self.fonts, self.scroll, scale);
        buffer.present().unwrap();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(format!("{} — mybrowser", self.page.title))
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
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
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
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                // Hit-test again at release time instead of trusting a stale hover.
                self.update_hover();
                if let Some(href) = self.hover.clone() {
                    self.follow(&href);
                }
            }
            WindowEvent::KeyboardInput { event, .. } if event.state == ElementState::Pressed => {
                let page = self.view_height() * 0.9;
                match &event.logical_key {
                    Key::Named(NamedKey::ArrowDown) => self.scroll_by(48.0),
                    Key::Named(NamedKey::ArrowUp) => self.scroll_by(-48.0),
                    Key::Named(NamedKey::PageDown) | Key::Named(NamedKey::Space) => self.scroll_by(page),
                    Key::Named(NamedKey::PageUp) => self.scroll_by(-page),
                    Key::Named(NamedKey::Home) => self.scroll_by(-1e9),
                    Key::Named(NamedKey::End) => self.scroll_by(1e9),
                    Key::Named(NamedKey::Backspace) => {
                        if let Some(prev) = self.history.pop() {
                            self.open(prev, false);
                        }
                    }
                    Key::Character(c) if c.eq_ignore_ascii_case("r") => {
                        let path = self.page.path.clone();
                        let keep = self.scroll;
                        self.open(path, false);
                        self.scroll = keep; // clamped on the next redraw
                    }
                    _ => {}
                }
            }
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
    fn link_resolution() {
        let base = Path::new("/site/pages");
        assert_eq!(resolve_link(base, "about.html"), Link::File("/site/pages/about.html".into()));
        assert_eq!(resolve_link(base, "a%20b.html?x=1#top"), Link::File("/site/pages/a b.html".into()));
        assert_eq!(resolve_link(base, "../up.html"), Link::File("/site/pages/../up.html".into()));
        assert_eq!(resolve_link(base, "sub/dir:odd.html"), Link::File("/site/pages/sub/dir:odd.html".into()));
        for nothing in ["", "#section", "  ", "?q=1"] {
            assert_eq!(resolve_link(base, nothing), Link::Nothing, "{nothing:?}");
        }
        for external in ["https://example.com", "mailto:a@b.c", "javascript:void(0)", "tel:123", "file:///etc/passwd", "//host/x", "data:text/html,hi"] {
            assert!(matches!(resolve_link(base, external), Link::External(_)), "{external}");
        }
    }
}
