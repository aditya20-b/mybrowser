//! Find in page. The query is searched for in the text the layout actually put
//! on screen (the display list), so what you find is exactly what you can see:
//! no hidden elements, collapsed whitespace as drawn. Pure state and search, no
//! drawing or windowing; `paint.rs` highlights the matches and `main.rs` feeds it keys.
//!
//! The layout emits one text item per word, so the search first stitches the words
//! of each visual line back together (a space wherever there is a gap). A match lies
//! within one line: a phrase that wraps onto the next line isn't found. Every block
//! and table cell starts with a `Nop` placeholder in the display list; that ends the line,
//! so a phrase can't run from one cell into the next.
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
/// A no-break space counts as an ordinary space.
fn fold(c: char) -> char {
    if c == '\u{a0}' {
        return ' ';
    }
    c.to_lowercase().next().unwrap_or(c)
}

/// Characters that are never drawn and have no width (zero-width spaces and joiners,
/// soft hyphens, byte-order marks, variation selectors, bidi embeddings and isolates,
/// invisible math operators): what you can't see, you can't find, and they must not
/// split a word you can.
fn is_ignorable(c: char) -> bool {
    matches!(
        c,
        '\u{ad}'
            | '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{feff}'
            | '\u{fe00}'..='\u{fe0f}'
    )
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
    /// Font size of the last word added (the space after it is in that font, or the
    /// next word's, whichever is smaller).
    last_size: f32,
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
    let needle: Vec<char> = query.chars().filter(|c| !is_ignorable(*c)).map(fold).collect();
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
        // the start of a block or table cell: whatever came before is a different line
        if matches!(item, Item::Nop) {
            flush(&mut line, &mut out);
            continue;
        }
        let Item::Text { x, baseline, text, size, face, .. } = item else { continue };
        // a word made only of invisible characters is nothing at all (not a gap)
        if text.chars().all(is_ignorable) {
            continue;
        }
        // the gap before a word is judged by the smaller of the two neighbouring font sizes, so
        // a space in small print next to a huge word is still a space
        let gap_unit = if line.last_size > 0.0 { size.min(line.last_size) } else { *size };
        // is this word the continuation of the line so far?
        let continues = !line.chars.is_empty()
            && (line.baseline - baseline).abs() < 0.5
            && *x >= *line.right.last().unwrap_or(&0.0) - 0.5
            && *x - *line.right.last().unwrap_or(&0.0) < size.max(line.last_size) * 2.0;
        if !continues {
            flush(&mut line, &mut out);
            line.baseline = *baseline;
            line.size = *size;
        } else if *x - *line.right.last().unwrap_or(&0.0) > gap_unit * 0.15 {
            // a gap between the words: a space, spanning it
            let from = *line.right.last().unwrap_or(x);
            line.chars.push(' ');
            line.left.push(from);
            line.right.push(*x);
        }
        line.last_size = *size;
        // one running pen position: every glyph's advance is added once (no kerning in this
        // engine), so a long word costs time in proportion to its length
        let mut pen = *x;
        let mut buf = [0u8; 4];
        for c in text.chars().filter(|c| !is_ignorable(*c)) {
            let advance = fonts.measure(*face, *size, c.encode_utf8(&mut buf));
            line.chars.push(c);
            line.left.push(pen);
            pen += advance;
            line.right.push(pen);
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
    fn a_space_in_small_print_next_to_a_huge_word_is_still_a_space() {
        let (layout, fonts) = laid_out("<p><span style='font-size:12px'>fine print</span> <span style='font-size:60px'>Big</span></p>", 800.0);
        assert_eq!(find_all(&layout, &fonts, "print big").len(), 1);
        assert!(find_all(&layout, &fonts, "printbig").is_empty());
        // and the other way round, as before
        let (layout, fonts) = laid_out("<p><span style='font-size:60px'>Big</span> <span style='font-size:12px'>fine print</span></p>", 800.0);
        assert_eq!(find_all(&layout, &fonts, "big fine").len(), 1);
        assert!(find_all(&layout, &fonts, "bigfine").is_empty());
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
        // cells that nearly touch used to be stitched together (they are a few pixels apart)
        let (layout, fonts) = laid_out("<table><tr><td>alpha</td><td>beta</td></tr></table>", 600.0);
        assert!(find_all(&layout, &fonts, "alpha beta").is_empty());
        assert!(find_all(&layout, &fonts, "alphabeta").is_empty());
        assert_eq!(find_all(&layout, &fonts, "beta").len(), 1);
        let (layout, fonts) = laid_out("<table cellpadding=0 cellspacing=0><tr><td>alpha</td><td>beta</td></tr></table>", 600.0);
        assert!(find_all(&layout, &fonts, "alphabeta").is_empty());
        // while text inside one cell, across inline elements, is still one phrase
        let (layout, fonts) = laid_out("<table><tr><td>alpha <b>beta</b></td></tr></table>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "alpha beta").len(), 1);
    }

    #[test]
    fn invisible_characters_neither_block_nor_create_matches() {
        // a zero-width space inside a word is not drawn, so "ab" is what you see
        let (layout, fonts) = laid_out("<p>a&#8203;b and c&#xFEFF;d</p>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "ab").len(), 1);
        assert_eq!(find_all(&layout, &fonts, "cd").len(), 1);
        // and typing one into the query doesn't change what matches
        assert_eq!(find_all(&layout, &fonts, "a\u{200b}b").len(), 1);
        assert!(find_all(&layout, &fonts, "\u{200b}").is_empty(), "a query of nothing visible finds nothing");
        // bidi controls and invisible operators are not drawn either
        let (layout, fonts) = laid_out("<p>a&#x202E;b c&#x2061;d e&#x202A;f</p>", 600.0);
        for q in ["ab", "cd", "ef"] {
            assert_eq!(find_all(&layout, &fonts, q).len(), 1, "{q}");
        }
        // bidi isolates too: they wrap a run of text, with a space before and after it
        let (layout, fonts) = laid_out("<p>a &#x2066;b&#x2069; c</p>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "a b c").len(), 1);
        // a lone zero-width space between words is not a second space
        let (layout, fonts) = laid_out("<p>a &#x200B; b</p>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "a b").len(), 1);
    }

    #[test]
    fn a_no_break_space_matches_a_normal_space() {
        let (layout, fonts) = laid_out("<p>price&nbsp;list and price list</p>", 600.0);
        assert_eq!(find_all(&layout, &fonts, "price list").len(), 2);
        assert_eq!(find_all(&layout, &fonts, "price\u{a0}list").len(), 2);
    }

    #[test]
    fn one_enormous_word_is_searched_in_linear_time() {
        // every character used to be measured from the start of the word: quadratic
        let (layout, fonts) = laid_out(&format!("<p style='width:100000px'>{}</p>", "x".repeat(20_000)), 600.0);
        let t = std::time::Instant::now();
        assert!(find_all(&layout, &fonts, "xy").is_empty());
        assert_eq!(find_all(&layout, &fonts, "xxxxxxxxxx").len(), 2000, "capped");
        assert!(t.elapsed().as_secs() < 10, "{:?}", t.elapsed());
        // highlight positions still add up: the match of the whole word spans its width
        let (layout, fonts) = laid_out("<style>body{margin:0}</style><p style='margin:0'>wide</p>", 600.0);
        let m = find_all(&layout, &fonts, "wide");
        assert!((m[0].w - fonts.measure(crate::fonts::Face { family: crate::fonts::Family::Sans, bold: false, italic: false }, m[0].size, "wide")).abs() < 0.5);
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
