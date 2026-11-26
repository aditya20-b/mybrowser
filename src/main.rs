mod css;
mod dom;
mod fonts;
mod html;
mod layout;
mod page;
mod paint;
mod style;

use fonts::Fonts;
use page::Page;
use paint::Painter;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::num::NonZeroU32;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut flag = |name: &str| -> Option<String> {
        let i = args.iter().position(|a| a == name)?;
        args.remove(i);
        (i < args.len()).then(|| args.remove(i))
    };
    let screenshot = flag("--screenshot");
    let width: f32 = flag("--width").and_then(|w| w.parse().ok()).unwrap_or(800.0);
    let scale: f32 = flag("--scale").and_then(|w| w.parse().ok()).unwrap_or(1.0);
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
    // Cap the image so a gigantic page can't allocate gigabytes.
    let max_h = 16_000.0;
    let (w, h) = ((width * scale) as usize, (page.layout.height.min(max_h) * scale).ceil().max(1.0) as usize);
    let mut buf = vec![0u32; w * h];
    Painter::default().paint(&mut buf, w, h, &page.layout, fonts, 0.0, scale);
    let t_paint = t0.elapsed() - t_load - t_layout;
    eprintln!("parse+style {t_load:?}, layout {t_layout:?}, paint {t_paint:?}");
    let rgb: Vec<u8> = buf.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    let file = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
    let mut enc = png::Encoder::new(file, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgb).unwrap();
    println!("wrote {} ({w}x{h}); {} display items", out.display(), page.layout.items.len());
}

/// Decode `%20`-style escapes in a URL path (invalid escapes are left as-is).
fn percent_decode(s: &str) -> String {
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
    String::from_utf8_lossy(&out).into_owned()
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

    fn scroll_by(&mut self, dy: f32) {
        let max = (self.page.layout.height - self.view_height()).max(0.0);
        self.scroll = (self.scroll + dy).clamp(0.0, max);
        self.window.as_ref().unwrap().request_redraw();
    }

    fn open(&mut self, path: PathBuf, push_history: bool) {
        if push_history {
            self.history.push(self.page.path.clone());
        }
        self.page = Page::load(&path);
        self.laid_out_width = -1.0; // force relayout
        self.scroll = 0.0;
        self.hover = None;
        if let Some(w) = &self.window {
            w.set_title(&format!("{} — mybrowser", if self.page.title.is_empty() { "untitled" } else { &self.page.title }));
            w.request_redraw();
        }
    }

    fn follow(&mut self, href: &str) {
        let target = href.split('#').next().unwrap_or("");
        if target.is_empty() {
            return; // in-page anchor (not supported yet)
        }
        if target.contains("://") {
            eprintln!("network loading not implemented yet: {target}");
            return;
        }
        let dir = self.page.path.parent().unwrap_or(Path::new("."));
        self.open(dir.join(percent_decode(target)), true);
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
            let max = (self.page.layout.height - self.view_height()).max(0.0);
            self.scroll = self.scroll.clamp(0.0, max);
        }
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
                self.window.as_ref().unwrap().request_redraw();
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
                let s = self.scale();
                self.hover = self.link_at(self.cursor.0 / s, self.cursor.1 / s + self.scroll);
                let icon = if self.hover.is_some() { CursorIcon::Pointer } else { CursorIcon::Default };
                self.window.as_ref().unwrap().set_cursor(icon);
            }
            WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
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
                        self.scroll = keep;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}
