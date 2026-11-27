//! Find in page. The query is searched for in the text the layout actually put
//! on screen (the display list), so what you find is exactly what you can see:
//! no hidden elements, collapsed whitespace as drawn. Pure state and search, no
//! drawing or windowing; `paint.rs` highlights the matches and `main.rs` feeds it keys.
//!
//! The layout emits one text item per word, so the search first stitches the words
//! of each visual line back together (a space wherever there is a gap). A match lies
//! within one line: a phrase that wraps onto the next line isn't found.
use crate::fonts::Fonts;
use crate::layout::{Item, Layout};

/// Longest query we accept.
pub const MAX_QUERY_CHARS: usize = 200;
/// Matches kept; more than this and the search stops (a page of "e"s).
pub const MAX_MATCHES: usize = 2000;

/// Where one occurrence of the query is, in layout (document) pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Match {
    pub x: f32,
    pub w: f32,
    /// y of the text baseline.
    pub baseline: f32,
    /// Font size of the line the match is on.
    pub size: f32,
}

#[derive(Debug, Default)]
pub struct Find {
    query: String,
    matches: Vec<Match>,
    current: usize,
}

/// Lower-case one character without changing how many characters there are,
/// so character positions in the folded text are positions in the original.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

impl Find {
    pub fn new() -> Find {
        Find::default()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn matches(&self) -> &[Match] {
        &self.matches
    }

    /// Which match is selected (0-based), if there are any.
    pub fn current_index(&self) -> Option<usize> {
        (!self.matches.is_empty()).then_some(self.current)
    }

    pub fn current_match(&self) -> Option<&Match> {
        self.matches.get(self.current)
    }

    /// Type (or paste) into the query. Control characters are dropped. Call
    /// `search` afterwards.
    pub fn type_text(&mut self, text: &str) {
        for c in text.chars().filter(|c| !c.is_control()) {
            if self.query.chars().count() < MAX_QUERY_CHARS {
                self.query.push(c);
            }
        }
    }

    pub fn backspace(&mut self) {
        self.query.pop();
    }

    /// Search again after the query changed: the first match becomes current.
    pub fn search(&mut self, layout: &Layout, fonts: &Fonts) {
        self.matches = find_all(layout, fonts, &self.query);
        self.current = 0;
    }

    /// Search again after the page was laid out differently (a resize): the
    /// selected match stays selected where possible.
    pub fn refresh(&mut self, layout: &Layout, fonts: &Fonts) {
        self.matches = find_all(layout, fonts, &self.query);
        self.current = self.current.min(self.matches.len().saturating_sub(1));
    }

    /// Select the next match, wrapping from the last to the first.
    pub fn next(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + 1) % self.matches.len();
        }
    }

    /// Select the previous match, wrapping from the first to the last.
    pub fn prev(&mut self) {
        if !self.matches.is_empty() {
            self.current = (self.current + self.matches.len() - 1) % self.matches.len();
        }
    }

    /// The scroll offset that brings the selected match into the upper part of a
    /// view `view_h` tall (the caller clamps it to the page).
    pub fn scroll_for_current(&self, view_h: f32) -> Option<f32> {
        self.current_match().map(|m| (m.baseline - m.size - view_h * 0.3).max(0.0))
    }
}

/// One visual line of text: its characters (as drawn, with a space between
/// words that have a gap) and where each one starts and ends horizontally.
#[derive(Default)]
struct Line {
    chars: Vec<char>,
    left: Vec<f32>,
    right: Vec<f32>,
    baseline: f32,
    size: f32,
}

/// Search one line for the (already folded) `needle`.
fn search_line(line: &Line, needle: &[char], out: &mut Vec<Match>) {
    if line.chars.len() < needle.len() {
        return;
    }
    let folded: Vec<char> = line.chars.iter().copied().map(fold).collect();
    let mut i = 0;
    while i + needle.len() <= folded.len() {
        if folded[i..i + needle.len()] == *needle {
            let (start, end) = (line.left[i], line.right[i + needle.len() - 1]);
            out.push(Match { x: start, w: (end - start).max(0.0), baseline: line.baseline, size: line.size });
            i += needle.len(); // matches don't overlap
        } else {
            i += 1;
        }
    }
}

/// Every occurrence of `query` (ignoring case) in the page's text, in reading order.
pub fn find_all(layout: &Layout, fonts: &Fonts, query: &str) -> Vec<Match> {
    let needle: Vec<char> = query.chars().map(fold).collect();
    let mut out = vec![];
    if needle.is_empty() {
        return out;
    }
    let mut line = Line::default();
    let flush = |line: &mut Line, out: &mut Vec<Match>| {
        search_line(line, &needle, out);
        *line = Line::default();
    };
    for item in &layout.items {
        let Item::Text { x, baseline, text, size, face, .. } = item else { continue };
        let chars: Vec<char> = text.chars().collect();
        if chars.is_empty() {
            continue;
        }
        // is this word the continuation of the line so far?
        let continues = !line.chars.is_empty()
            && (line.baseline - baseline).abs() < 0.5
            && *x >= *line.right.last().unwrap_or(&0.0) - 0.5
            && *x - *line.right.last().unwrap_or(&0.0) < size * 2.0;
        if !continues {
            flush(&mut line, &mut out);
            line.baseline = *baseline;
            line.size = *size;
        } else if *x - *line.right.last().unwrap_or(&0.0) > size * 0.15 {
            // a gap between the words: a space, spanning it
            let from = *line.right.last().unwrap_or(x);
            line.chars.push(' ');
            line.left.push(from);
            line.right.push(*x);
        }
        for k in 0..chars.len() {
            let before: String = chars[..k].iter().collect();
            let upto: String = chars[..=k].iter().collect();
            line.chars.push(chars[k]);
            line.left.push(x + fonts.measure(*face, *size, &before));
            line.right.push(x + fonts.measure(*face, *size, &upto));
        }
        if out.len() >= MAX_MATCHES {
            break;
        }
    }
    flush(&mut line, &mut out);
    out.truncate(MAX_MATCHES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;
    use url::Url;

    fn laid_out(src: &str, width: f32) -> (Layout, Fonts) {
        let fonts = Fonts::load();
        let mut page = Page::from_source(&Url::parse("file:///tmp/t.html").unwrap(), src);
        page.relayout(&fonts, width);
        (page.layout, fonts)
    }

    #[test]
    fn finds_every_occurrence_ignoring_case_in_reading_order() {
        let (layout, fonts) = laid_out("<p>The cat sat. A CAT, a Cat, a dog.</p><p>Another cat</p>", 600.0);
        let m = find_all(&layout, &fonts, "cat");
        assert_eq!(m.len(), 4);
        // in order: left to right on a line, then down the page
        for pair in m.windows(2) {
            assert!(pair[0].baseline < pair[1].baseline || (pair[0].baseline == pair[1].baseline && pair[0].x < pair[1].x), "{pair:?}");
        }
        assert!(m.iter().all(|m| m.w > 0.0 && m.size > 0.0));
        assert_eq!(find_all(&layout, &fonts, "CAT").len(), 4, "the query's case doesn't matter either");
        assert_eq!(find_all(&layout, &fonts, "dog").len(), 1);
        assert!(find_all(&layout, &fonts, "zebra").is_empty());
        assert!(find_all(&layout, &fonts, "").is_empty(), "an empty query matches nothing");
    }

    #[test]
    fn a_match_sits_where_its_text_is_drawn() {
        let (layout, fonts) = laid_out("<style>body{margin:0}</style><p style='margin:0'>hello world</p>", 600.0);
        let word = find_all(&layout, &fonts, "world");
        let all = find_all(&layout, &fonts, "hello world");
        assert_eq!((word.len(), all.len()), (1, 1), "a phrase of several words is found");
        assert!(word[0].x > all[0].x + 10.0, "world starts after hello: {} vs {}", word[0].x, all[0].x);
        assert!((word[0].x + word[0].w - (all[0].x + all[0].w)).abs() < 1.0, "and ends where the whole phrase ends");
        assert_eq!(word[0].baseline, all[0].baseline);
        assert!(all[0].w > word[0].w * 1.5, "the phrase is wider than one word");
    }

    #[test]
    fn phrases_are_found_across_inline_elements_but_not_across_lines() {
        let (layout, fonts) = laid_out("<p>foo <b>bar</b> <a href=x>baz</a>qux</p>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "foo bar baz").len(), 1, "bold and link boundaries don't split a phrase");
        assert_eq!(find_all(&layout, &fonts, "bazqux").len(), 1, "touching words stay joined");
        assert!(find_all(&layout, &fonts, "foo  bar").is_empty(), "one space in the page, one in the query");
        // a narrow column wraps the phrase onto two lines: no single line holds it
        let (narrow, fonts) = laid_out("<p>alpha beta gamma delta</p>", 90.0);
        assert!(find_all(&narrow, &fonts, "alpha beta gamma delta").is_empty());
        assert_eq!(find_all(&narrow, &fonts, "gamma").len(), 1);
    }

    #[test]
    fn hidden_text_is_not_found_and_overlapping_matches_are_not_double_counted() {
        let (layout, fonts) = laid_out("<p>visible</p><p style='display:none'>secret secret</p><p>aaaa</p>", 600.0);
        assert!(find_all(&layout, &fonts, "secret").is_empty(), "display:none text isn't on screen");
        assert_eq!(find_all(&layout, &fonts, "aa").len(), 2, "aaaa holds two non-overlapping aa");
        assert_eq!(find_all(&layout, &fonts, "visible").len(), 1);
    }

    #[test]
    fn matches_wrap_with_their_lines_and_unicode_is_safe() {
        let (layout, fonts) = laid_out("<p>ÉCOLE école Straße ΣΑΣ 日本語 🎉🎉 word word word word word word word word</p>", 120.0);
        assert_eq!(find_all(&layout, &fonts, "école").len(), 2, "case folding works beyond ASCII");
        assert_eq!(find_all(&layout, &fonts, "日本").len(), 1);
        assert_eq!(find_all(&layout, &fonts, "🎉").len(), 2);
        let words = find_all(&layout, &fonts, "word");
        assert_eq!(words.len(), 8);
        let lines: std::collections::BTreeSet<i32> = words.iter().map(|m| m.baseline as i32).collect();
        assert!(lines.len() > 1, "the text wrapped, so the matches are on several lines");
        for q in ["ß", "σ", "\u{0}", "a\u{301}", &"x".repeat(1000)] {
            let _ = find_all(&layout, &fonts, q);
        }
    }

    #[test]
    fn words_in_separate_table_cells_are_not_one_phrase() {
        let (layout, fonts) = laid_out("<table><tr><td style='width:200px'>alpha</td><td>beta</td></tr></table>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "alpha").len(), 1);
        assert_eq!(find_all(&layout, &fonts, "beta").len(), 1);
        assert!(find_all(&layout, &fonts, "alpha beta").is_empty(), "a wide gap ends the line");
    }

    #[test]
    fn the_match_count_is_capped() {
        let (layout, fonts) = laid_out(&format!("<p>{}</p>", "e ".repeat(5000)), 600.0);
        assert_eq!(find_all(&layout, &fonts, "e").len(), MAX_MATCHES);
    }

    #[test]
    fn typing_editing_and_stepping_through_matches() {
        let (layout, fonts) = laid_out("<p>one cat</p><p>two cats</p><p>three cats and a cat</p>", 600.0);
        let mut find = Find::new();
        assert_eq!((find.current_index(), find.current_match()), (None, None));
        find.type_text("ca\n\t\u{1b}t");
        assert_eq!(find.query(), "cat", "control characters are dropped");
        find.search(&layout, &fonts);
        assert_eq!(find.matches().len(), 4);
        assert_eq!(find.current_index(), Some(0));
        find.next();
        find.next();
        assert_eq!(find.current_index(), Some(2));
        find.next();
        find.next();
        assert_eq!(find.current_index(), Some(0), "wraps forward");
        find.prev();
        assert_eq!(find.current_index(), Some(3), "and backward");
        // editing the query starts again from the first match
        find.backspace();
        find.type_text("s");
        assert_eq!(find.query(), "cas");
        find.search(&layout, &fonts);
        assert_eq!((find.matches().len(), find.current_index()), (0, None));
        find.next();
        find.prev(); // nothing to step through, and no panic
        // a refresh keeps the selection where possible
        find.backspace();
        find.type_text("t");
        find.search(&layout, &fonts);
        find.next();
        find.next();
        find.refresh(&layout, &fonts);
        assert_eq!(find.current_index(), Some(2));
        let (narrow, _) = laid_out("<p>one cat</p>", 600.0);
        find.refresh(&narrow, &fonts);
        assert_eq!(find.current_index(), Some(0), "the selection is clamped when matches disappear");
    }

    #[test]
    fn the_view_scrolls_to_put_the_match_near_the_top() {
        let filler = "<p style='margin:0;height:50px'>filler</p>".repeat(40);
        let (layout, fonts) = laid_out(&format!("{filler}<p>needle</p>"), 600.0);
        let mut find = Find::new();
        find.type_text("needle");
        find.search(&layout, &fonts);
        let m = *find.current_match().unwrap();
        assert!(m.baseline > 1500.0, "{}", m.baseline);
        let scroll = find.scroll_for_current(600.0).unwrap();
        // the match ends up in the upper part of the view
        let y_in_view = m.baseline - scroll;
        assert!(y_in_view > 0.0 && y_in_view < 300.0, "{y_in_view}");
        // a match near the top of the page doesn't scroll at all
        let mut top = Find::new();
        top.type_text("filler");
        top.search(&layout, &fonts);
        assert_eq!(top.scroll_for_current(600.0), Some(0.0));
        assert_eq!(Find::new().scroll_for_current(600.0), None);
    }

    #[test]
    fn the_query_length_is_capped() {
        let mut find = Find::new();
        find.type_text(&"x".repeat(MAX_QUERY_CHARS * 3));
        assert_eq!(find.query().chars().count(), MAX_QUERY_CHARS);
        for _ in 0..MAX_QUERY_CHARS * 2 {
            find.backspace();
        }
        assert_eq!(find.query(), "");
    }
}
