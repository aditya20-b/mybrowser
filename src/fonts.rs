//! Font loading and metrics (rasterization is done by `fontdue`).
use fontdue::{Font, FontSettings};

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

pub struct Fonts {
    fonts: Vec<Font>,
}

const DIR: &str = "/System/Library/Fonts/Supplemental/";

impl Fonts {
    pub fn load() -> Fonts {
        let families = [
            ["Arial", "Arial Italic", "Arial Bold", "Arial Bold Italic"],
            ["Times New Roman", "Times New Roman Italic", "Times New Roman Bold", "Times New Roman Bold Italic"],
            ["Courier New", "Courier New Italic", "Courier New Bold", "Courier New Bold Italic"],
        ];
        // order within a family: [regular, italic, bold, bold-italic] to match Face::index
        let read = |name: &str| -> Option<Font> {
            let bytes = std::fs::read(format!("{DIR}{name}.ttf")).ok()?;
            Font::from_bytes(bytes, FontSettings::default()).ok()
        };
        let fallback = read("Arial").expect("could not load Arial from system fonts");
        let mut fonts = vec![];
        for fam in families {
            for name in fam {
                fonts.push(read(name).unwrap_or_else(|| fallback.clone()));
            }
        }
        Fonts { fonts }
    }

    pub fn font(&self, face: Face) -> &Font {
        &self.fonts[face.index()]
    }

    pub fn measure(&self, face: Face, size: f32, text: &str) -> f32 {
        let font = self.font(face);
        text.chars().map(|c| font.metrics(c, size).advance_width).sum()
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
