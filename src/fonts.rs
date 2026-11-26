//! Font loading and metrics (rasterization is done by `fontdue`).
use fontdue::{Font, FontSettings};
use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Family {
    Sans,
    Serif,
    Mono,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Face {
    pub family: Family,
    pub bold: bool,
    pub italic: bool,
}

impl Face {
    pub fn index(self) -> usize {
        (self.family as usize) * 4 + (self.bold as usize) * 2 + self.italic as usize
    }
}

/// Zero-width and formatting characters that must take no space and draw
/// nothing (the fonts have no glyph for them, so they would show as boxes or,
/// for the soft hyphen, as a visible "-"). Layout and paint must both skip
/// exactly these so measured and drawn widths agree.
pub fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{AD}'
            | '\u{FEFF}'
    )
}

/// Glyph-cache index used for the fallback font (after the 12 face slots).
pub const FALLBACK_INDEX: usize = 12;

pub struct Fonts {
    fonts: Vec<Font>,
}

const DIR: &str = "/System/Library/Fonts/Supplemental/";

fn read_font(name: &str) -> Option<Font> {
    let bytes = std::fs::read(format!("{DIR}{name}.ttf")).ok()?;
    Font::from_bytes(bytes, FontSettings::default()).ok()
}

/// A wide-coverage font (arrows, symbols, CJK ...) for characters the main
/// faces lack. It is large, so it is loaded once, on first use.
fn fallback_font() -> Option<&'static Font> {
    static FALLBACK: OnceLock<Option<Font>> = OnceLock::new();
    FALLBACK.get_or_init(|| read_font("Arial Unicode")).as_ref()
}

impl Fonts {
    pub fn load() -> Fonts {
        let families = [
            ["Arial", "Arial Italic", "Arial Bold", "Arial Bold Italic"],
            ["Times New Roman", "Times New Roman Italic", "Times New Roman Bold", "Times New Roman Bold Italic"],
            ["Courier New", "Courier New Italic", "Courier New Bold", "Courier New Bold Italic"],
        ];
        // order within a family: [regular, italic, bold, bold-italic] to match Face::index
        let fallback = read_font("Arial").expect("could not load Arial from system fonts");
        let mut fonts = vec![];
        for fam in families {
            for name in fam {
                fonts.push(read_font(name).unwrap_or_else(|| fallback.clone()));
            }
        }
        Fonts { fonts }
    }

    pub fn font(&self, face: Face) -> &Font {
        &self.fonts[face.index()]
    }

    /// The font to draw `ch` with, and its glyph-cache index: the face itself
    /// if it has a glyph, else the wide-coverage fallback, else the face
    /// (which then draws its "missing glyph" box).
    pub fn resolve(&self, face: Face, ch: char) -> (usize, &Font) {
        let primary = self.font(face);
        if primary.lookup_glyph_index(ch) != 0 {
            return (face.index(), primary);
        }
        match fallback_font() {
            Some(fb) if fb.lookup_glyph_index(ch) != 0 => (FALLBACK_INDEX, fb),
            _ => (face.index(), primary),
        }
    }

    pub fn measure(&self, face: Face, size: f32, text: &str) -> f32 {
        text.chars()
            .filter(|c| !invisible(*c))
            .map(|c| self.resolve(face, c).1.metrics(c, size).advance_width)
            .sum()
    }

    /// (ascent, descent) in pixels; descent includes line gap so
    /// ascent + descent is the natural line height.
    pub fn vmetrics(&self, face: Face, size: f32) -> (f32, f32) {
        match self.font(face).horizontal_line_metrics(size) {
            Some(m) => (m.ascent, m.new_line_size - m.ascent),
            None => (size * 0.9, size * 0.3),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sans() -> Face {
        Face { family: Family::Sans, bold: false, italic: false }
    }

    #[test]
    fn invisible_characters_take_no_space() {
        let fonts = Fonts::load();
        let plain = fonts.measure(sans(), 20.0, "ab");
        for s in ["a\u{200B}b", "a\u{200D}b", "a\u{FE0F}b", "a\u{AD}b", "\u{FEFF}ab\u{200E}"] {
            assert_eq!(fonts.measure(sans(), 20.0, s), plain, "{s:?}");
        }
        assert!(fonts.measure(sans(), 20.0, "a\u{00A0}b") > plain, "nbsp is a real space");
    }

    #[test]
    fn missing_glyphs_fall_back_to_a_wide_coverage_font() {
        let fonts = Fonts::load();
        // plain letters use the face itself
        assert_eq!(fonts.resolve(sans(), 'a').0, sans().index());
        // CJK and the reload arrow aren't in Arial; the fallback has them (when installed)
        if fallback_font().is_some() {
            assert_eq!(fonts.resolve(sans(), '日').0, FALLBACK_INDEX);
            assert_eq!(fonts.resolve(sans(), '↻').0, FALLBACK_INDEX);
            assert!(fonts.measure(sans(), 20.0, "日本") > 20.0, "CJK now has real width");
        }
        // a char nobody has still resolves (to the face's own missing-glyph box)
        assert_eq!(fonts.resolve(sans(), '\u{10FFFE}').0, sans().index());
    }
}
