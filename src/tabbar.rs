//! The tab strip above the toolbar: geometry, hit testing and which tab to
//! select after one is closed. Pure functions with no drawing or windowing, so
//! they can be unit-tested; `paint.rs` draws the strip and `main.rs` feeds it
//! mouse clicks.

/// Height of the strip in logical pixels.
pub const HEIGHT: f32 = 32.0;
/// Most tabs open at once (each holds a whole page in memory).
pub const MAX_TABS: usize = 20;
const PADDING: f32 = 8.0;
const TOP_GAP: f32 = 4.0;
const MAX_TAB_WIDTH: f32 = 220.0;
/// Narrow enough that `MAX_TABS` tabs and the + button fit a 600 px window; at this
/// width a tab is just its close button.
const MIN_TAB_WIDTH: f32 = 28.0;
const CLOSE_SIZE: f32 = 16.0;
const NEW_BUTTON: f32 = 24.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

fn inside(r: Rect, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TabHit {
    /// Select tab `n`.
    Select(usize),
    /// The close button of tab `n`.
    Close(usize),
    /// The "+" button.
    New,
}

/// What the painter needs to draw the strip.
pub struct StripView<'a> {
    pub titles: &'a [String],
    pub loading: &'a [bool],
    pub active: usize,
}

/// Width of each tab when `count` tabs share a window `width` wide.
pub fn tab_width(count: usize, width: f32) -> f32 {
    let room = (width - 2.0 * PADDING - NEW_BUTTON).max(0.0);
    (room / count.max(1) as f32).clamp(MIN_TAB_WIDTH, MAX_TAB_WIDTH)
}

pub fn tab_rect(index: usize, count: usize, width: f32) -> Rect {
    let w = tab_width(count, width);
    Rect { x: PADDING + index as f32 * w, y: TOP_GAP, w: w - 2.0, h: HEIGHT - TOP_GAP }
}

pub fn close_rect(index: usize, count: usize, width: f32) -> Rect {
    let t = tab_rect(index, count, width);
    Rect { x: t.x + t.w - CLOSE_SIZE - 8.0, y: t.y + (t.h - CLOSE_SIZE) / 2.0, w: CLOSE_SIZE, h: CLOSE_SIZE }
}

pub fn new_rect(count: usize, width: f32) -> Rect {
    Rect { x: PADDING + count as f32 * tab_width(count, width) + 4.0, y: TOP_GAP + (HEIGHT - TOP_GAP - NEW_BUTTON) / 2.0, w: NEW_BUTTON, h: NEW_BUTTON }
}

/// Which part of the strip is at logical `(x, y)` (`y` measured from the top of the window).
/// Parts that fall outside a (very narrow) window can't be hit.
pub fn hit_test(x: f32, y: f32, count: usize, width: f32) -> Option<TabHit> {
    if !(0.0..HEIGHT).contains(&y) || !(x < width) {
        return None;
    }
    for i in 0..count {
        if inside(close_rect(i, count, width), x, y) {
            return Some(TabHit::Close(i));
        }
        if inside(tab_rect(i, count, width), x, y) {
            return Some(TabHit::Select(i));
        }
    }
    inside(new_rect(count, width), x, y).then_some(TabHit::New)
}

/// The tab after (or before) `active`, wrapping around.
pub fn cycle(active: usize, count: usize, forward: bool) -> usize {
    if count == 0 {
        return 0;
    }
    if forward { (active + 1) % count } else { (active + count - 1) % count }
}

/// Which tab is active after tab `closed` is removed, leaving `remaining` tabs.
/// Closing a tab to the left shifts the active one down; closing the active tab
/// selects the one that took its place (or the last, if it was the last).
pub fn active_after_close(active: usize, closed: usize, remaining: usize) -> usize {
    let next = if closed < active { active - 1 } else { active };
    next.min(remaining.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn centre(r: Rect) -> (f32, f32) {
        (r.x + r.w / 2.0, r.y + r.h / 2.0)
    }

    #[test]
    fn tabs_share_the_width_within_limits() {
        assert_eq!(tab_width(1, 1000.0), MAX_TAB_WIDTH, "a lone tab doesn't stretch across the window");
        assert_eq!(tab_width(4, 600.0), (600.0 - 2.0 * PADDING - NEW_BUTTON) / 4.0);
        assert_eq!(tab_width(MAX_TABS, 300.0), MIN_TAB_WIDTH, "they stop shrinking at a minimum");
        assert_eq!(tab_width(0, 500.0), MAX_TAB_WIDTH, "no tabs doesn't divide by zero");
        assert_eq!(tab_width(3, 0.0), MIN_TAB_WIDTH);
    }

    #[test]
    fn clicks_select_close_or_add_tabs() {
        let (count, width) = (3, 800.0);
        for i in 0..count {
            let (x, y) = centre(tab_rect(i, count, width));
            // the middle of a tab is the title area; the close button sits at its right end
            assert_eq!(hit_test(x - 20.0, y, count, width), Some(TabHit::Select(i)));
            let (cx, cy) = centre(close_rect(i, count, width));
            assert_eq!(hit_test(cx, cy, count, width), Some(TabHit::Close(i)));
        }
        let (nx, ny) = centre(new_rect(count, width));
        assert_eq!(hit_test(nx, ny, count, width), Some(TabHit::New));
        assert_eq!(hit_test(nx + 100.0, ny, count, width), None, "empty strip space");
        assert_eq!(hit_test(100.0, HEIGHT + 1.0, count, width), None, "below the strip");
        assert_eq!(hit_test(100.0, -1.0, count, width), None);
        // tabs don't overlap each other, and the + button is after the last tab
        for i in 0..count - 1 {
            let (a, b) = (tab_rect(i, count, width), tab_rect(i + 1, count, width));
            assert!(a.x + a.w <= b.x);
        }
        let last = tab_rect(count - 1, count, width);
        assert!(last.x + last.w <= new_rect(count, width).x);
        // the close button is inside its tab
        let (t, c) = (tab_rect(0, count, width), close_rect(0, count, width));
        assert!(c.x >= t.x && c.x + c.w <= t.x + t.w && c.y >= t.y && c.y + c.h <= t.y + t.h);
    }

    #[test]
    fn every_tab_and_the_plus_button_stay_reachable_in_a_modest_window() {
        // with 20 tabs in a 700 px window the strip used to run past the right edge
        let (count, width) = (MAX_TABS, 700.0);
        for i in 0..count {
            let t = tab_rect(i, count, width);
            assert!(t.x + t.w <= width, "tab {i} ends at {}", t.x + t.w);
            let (cx, cy) = centre(close_rect(i, count, width));
            assert_eq!(hit_test(cx, cy, count, width), Some(TabHit::Close(i)));
            // the close button stays inside its own (narrow) tab and clear of the next one
            let (c, t) = (close_rect(i, count, width), tab_rect(i, count, width));
            assert!(c.x >= t.x && c.x + c.w <= t.x + t.w, "tab {i}");
        }
        let plus = new_rect(count, width);
        assert!(plus.x + plus.w <= width);
        let (px, py) = centre(plus);
        assert_eq!(hit_test(px, py, count, width), Some(TabHit::New));
        // in a window too narrow for the strip, parts beyond the right edge can't be clicked
        assert_eq!(hit_test(width + 5.0, 10.0, count, width), None);
        let narrow = 200.0;
        let (lx, ly) = centre(tab_rect(count - 1, count, narrow));
        assert!(lx > narrow, "the last tab is off screen here");
        assert_eq!(hit_test(lx, ly, count, narrow), None);
    }

    #[test]
    fn nonsense_input_never_panics() {
        for v in [f32::NAN, f32::INFINITY, -1.0, 1e30, 0.0] {
            for count in [0, 1, 7, MAX_TABS] {
                let _ = hit_test(v, v, count, v);
                let _ = tab_rect(3, count, v);
                let _ = close_rect(3, count, v);
                let _ = new_rect(count, v);
            }
        }
    }

    #[test]
    fn cycling_wraps_both_ways() {
        assert_eq!(cycle(0, 3, true), 1);
        assert_eq!(cycle(2, 3, true), 0);
        assert_eq!(cycle(0, 3, false), 2);
        assert_eq!(cycle(1, 3, false), 0);
        assert_eq!(cycle(0, 1, true), 0);
        assert_eq!(cycle(5, 0, true), 0);
    }

    #[test]
    fn closing_a_tab_keeps_the_right_tab_selected() {
        // tabs A B C D, closing one of them (3 remain afterwards for the first four cases)
        assert_eq!(active_after_close(2, 0, 3), 1, "a tab to the left closed: C is now at index 1");
        assert_eq!(active_after_close(1, 3, 3), 1, "a tab to the right closed: nothing moves");
        assert_eq!(active_after_close(1, 1, 3), 1, "the active tab closed: its right neighbour takes the place");
        assert_eq!(active_after_close(3, 3, 3), 2, "the last tab closed: select the new last");
        assert_eq!(active_after_close(0, 0, 0), 0, "no tabs left doesn't underflow");
    }
}
