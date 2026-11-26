//! Browser chrome: the toolbar with back / forward / reload buttons and the
//! address bar. This module is pure state and geometry (no drawing, no
//! windowing), so it can be unit-tested; `paint.rs` draws it and `main.rs`
//! feeds it input.

/// Toolbar height in logical pixels.
pub const HEIGHT: f32 = 44.0;
const BUTTON: f32 = 30.0;
const PADDING: f32 = 8.0;
const GAP: f32 = 6.0;
/// Longest address we accept, so pasting a megabyte can't stall painting.
pub const MAX_ADDRESS_CHARS: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Back,
    Forward,
    Reload,
    Address,
}

fn button(index: usize) -> Rect {
    Rect { x: PADDING + index as f32 * (BUTTON + GAP), y: (HEIGHT - BUTTON) / 2.0, w: BUTTON, h: BUTTON }
}

pub fn back_rect() -> Rect {
    button(0)
}

pub fn forward_rect() -> Rect {
    button(1)
}

pub fn reload_rect() -> Rect {
    button(2)
}

/// The address field fills the rest of the toolbar (zero width if the window is tiny).
pub fn address_rect(width: f32) -> Rect {
    let x = PADDING + 3.0 * (BUTTON + GAP);
    Rect { x, y: 6.0, w: (width - x - PADDING).max(0.0), h: HEIGHT - 12.0 }
}

/// Which toolbar element is at logical position `(x, y)` in a window `width` wide.
pub fn hit_test(x: f32, y: f32, width: f32) -> Option<Hit> {
    if back_rect().contains(x, y) {
        Some(Hit::Back)
    } else if forward_rect().contains(x, y) {
        Some(Hit::Forward)
    } else if reload_rect().contains(x, y) {
        Some(Hit::Reload)
    } else if address_rect(width).contains(x, y) {
        Some(Hit::Address)
    } else {
        None
    }
}

/// Editable address field. The caret is a *character* index (not a byte
/// index), so multi-byte text can't be split mid-character.
#[derive(Debug, Default)]
pub struct AddressBar {
    text: String,
    caret: usize,
    select_all: bool,
    focused: bool,
    /// A message shown in place of the text after a failed navigation attempt.
    pub error: Option<String>,
}

impl AddressBar {
    pub fn new(url: &str) -> AddressBar {
        let mut bar = AddressBar::default();
        bar.show(url);
        bar
    }

    /// Display `url` and leave editing mode (after navigation or Escape).
    pub fn show(&mut self, url: &str) {
        self.text = url.chars().take(MAX_ADDRESS_CHARS).collect();
        self.caret = self.len();
        self.select_all = false;
        self.focused = false;
        self.error = None;
    }

    /// Start editing with the whole address selected, like Cmd+L.
    pub fn focus(&mut self) {
        self.focused = true;
        self.select_all = true;
        self.caret = self.len();
        self.error = None;
    }

    /// A second click in the field: keep editing, caret at the end, nothing selected.
    pub fn click_again(&mut self) {
        self.select_all = false;
        self.caret = self.len();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn caret(&self) -> usize {
        self.caret
    }

    pub fn is_focused(&self) -> bool {
        self.focused
    }

    pub fn is_all_selected(&self) -> bool {
        self.select_all
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.text.char_indices().nth(char_index).map_or(self.text.len(), |(i, _)| i)
    }

    /// Replace a selection (if any) before an edit.
    fn drop_selection(&mut self) {
        if self.select_all {
            self.text.clear();
            self.caret = 0;
            self.select_all = false;
        }
    }

    /// Type or paste text. Control characters (newlines, tabs, escapes) are dropped.
    pub fn insert(&mut self, s: &str) {
        self.error = None;
        self.drop_selection();
        for c in s.chars().filter(|c| !c.is_control()) {
            if self.len() >= MAX_ADDRESS_CHARS {
                break;
            }
            let at = self.byte_index(self.caret);
            self.text.insert(at, c);
            self.caret += 1;
        }
    }

    pub fn backspace(&mut self) {
        self.error = None;
        if self.select_all {
            return self.drop_selection();
        }
        if self.caret > 0 {
            let at = self.byte_index(self.caret - 1);
            self.text.remove(at);
            self.caret -= 1;
        }
    }

    pub fn delete(&mut self) {
        self.error = None;
        if self.select_all {
            return self.drop_selection();
        }
        if self.caret < self.len() {
            let at = self.byte_index(self.caret);
            self.text.remove(at);
        }
    }

    pub fn clear(&mut self) {
        self.error = None;
        self.select_all = true;
        self.drop_selection();
    }

    pub fn left(&mut self) {
        if self.select_all {
            self.select_all = false;
            self.caret = 0;
        } else {
            self.caret = self.caret.saturating_sub(1);
        }
    }

    pub fn right(&mut self) {
        if self.select_all {
            self.select_all = false;
            self.caret = self.len();
        } else {
            self.caret = (self.caret + 1).min(self.len());
        }
    }

    pub fn home(&mut self) {
        self.select_all = false;
        self.caret = 0;
    }

    pub fn end(&mut self) {
        self.select_all = false;
        self.caret = self.len();
    }

    pub fn select_all(&mut self) {
        self.select_all = !self.text.is_empty();
        self.caret = self.len();
    }
}

/// Everything the painter needs to draw the toolbar.
pub struct ChromeView<'a> {
    pub bar: &'a AddressBar,
    pub can_back: bool,
    pub can_forward: bool,
}

// ---------- clipboard (macOS: pbcopy / pbpaste) -------------------------------

#[cfg(target_os = "macos")]
pub fn clipboard_get() -> Option<String> {
    let out = std::process::Command::new("pbpaste").output().ok()?;
    String::from_utf8(out.stdout).ok()
}

#[cfg(target_os = "macos")]
pub fn clipboard_set(text: &str) {
    use std::io::Write;
    if let Ok(mut child) = std::process::Command::new("pbcopy").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard_get() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
pub fn clipboard_set(_text: &str) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_replaces_the_selected_address_then_appends() {
        let mut bar = AddressBar::new("https://old.example/");
        assert!(!bar.is_focused());
        bar.focus();
        assert!(bar.is_focused() && bar.is_all_selected());
        bar.insert("exa");
        bar.insert("mple.com");
        assert_eq!((bar.text(), bar.caret()), ("example.com", 11));
        assert!(!bar.is_all_selected());
    }

    #[test]
    fn editing_in_the_middle() {
        let mut bar = AddressBar::new("abcd");
        bar.focus();
        bar.click_again(); // caret at end, no selection
        bar.left();
        bar.left();
        bar.insert("X");
        assert_eq!((bar.text(), bar.caret()), ("abXcd", 3));
        bar.backspace();
        bar.delete();
        assert_eq!((bar.text(), bar.caret()), ("abd", 2));
        bar.home();
        bar.backspace(); // nothing before the caret
        bar.end();
        bar.delete(); // nothing after the caret
        assert_eq!(bar.text(), "abd");
    }

    #[test]
    fn multibyte_text_never_panics_or_splits_characters() {
        let mut bar = AddressBar::new("");
        bar.focus();
        bar.insert("é日本🎉語");
        assert_eq!((bar.len(), bar.caret()), (5, 5));
        bar.left();
        bar.left();
        bar.insert("→");
        assert_eq!(bar.text(), "é日本→🎉語");
        bar.backspace();
        bar.backspace();
        bar.delete();
        assert_eq!(bar.text(), "é日語");
        for _ in 0..10 {
            bar.left();
        }
        bar.delete();
        assert_eq!(bar.text(), "日語");
        for _ in 0..10 {
            bar.backspace();
            bar.delete();
            bar.right();
        }
    }

    #[test]
    fn control_characters_are_dropped_and_length_is_capped() {
        let mut bar = AddressBar::new("");
        bar.focus();
        bar.insert("a\nb\tc\u{1b}d\r");
        assert_eq!(bar.text(), "abcd");
        bar.insert(&"x".repeat(MAX_ADDRESS_CHARS * 2));
        assert_eq!(bar.len(), MAX_ADDRESS_CHARS);
        let long = AddressBar::new(&"y".repeat(MAX_ADDRESS_CHARS + 50));
        assert_eq!(long.len(), MAX_ADDRESS_CHARS);
    }

    #[test]
    fn selection_behaviour_for_arrows_delete_and_select_all() {
        let mut bar = AddressBar::new("hello");
        bar.focus();
        bar.left();
        assert_eq!((bar.caret(), bar.is_all_selected()), (0, false));
        bar.select_all();
        bar.right();
        assert_eq!((bar.caret(), bar.is_all_selected()), (5, false));
        bar.select_all();
        bar.backspace();
        assert_eq!(bar.text(), "");
        bar.select_all(); // nothing to select in an empty field
        assert!(!bar.is_all_selected());
        // clear() empties regardless of the caret
        let mut bar = AddressBar::new("abc");
        bar.focus();
        bar.click_again();
        bar.clear();
        assert_eq!((bar.text(), bar.caret()), ("", 0));
    }

    #[test]
    fn show_leaves_editing_mode_and_clears_errors() {
        let mut bar = AddressBar::new("a");
        bar.focus();
        bar.error = Some("nope".into());
        bar.insert("b");
        assert!(bar.error.is_none(), "typing clears the error");
        bar.error = Some("again".into());
        bar.show("https://x.test/");
        assert!(!bar.is_focused() && bar.error.is_none());
        assert_eq!(bar.text(), "https://x.test/");
    }

    #[test]
    fn hit_testing() {
        let w = 900.0;
        let centre = |r: Rect| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        for (rect, hit) in [(back_rect(), Hit::Back), (forward_rect(), Hit::Forward), (reload_rect(), Hit::Reload), (address_rect(w), Hit::Address)] {
            let (x, y) = centre(rect);
            assert_eq!(hit_test(x, y, w), Some(hit));
        }
        assert_eq!(hit_test(0.0, 0.0, w), None);
        assert_eq!(hit_test(500.0, HEIGHT + 1.0, w), None, "below the toolbar");
        assert_eq!(hit_test(w - 1.0, 20.0, w), None, "the padding after the field");
        // buttons don't overlap each other or the field
        assert!(back_rect().x + back_rect().w <= forward_rect().x);
        assert!(forward_rect().x + forward_rect().w <= reload_rect().x);
        assert!(reload_rect().x + reload_rect().w <= address_rect(w).x);
        // a tiny window gives a zero-width field instead of a negative one
        assert_eq!(address_rect(10.0).w, 0.0);
        assert_eq!(hit_test(150.0, 20.0, 10.0), None);
    }
}
