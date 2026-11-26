# mybrowser

I wanted to understand what a browser actually does, so I'm writing one from scratch in Rust. The goal is a *functional* browser: type an address, fetch the page over HTTPS, parse the HTML and CSS, lay it out, and paint it. There's no WebKit or Blink underneath and no UI toolkit doing the text layout for me. Only the window, the TLS stack and the font rasterizer come from crates.

It is early. It can load and read plain, document-style websites today, including older table-based ones like Hacker News, and it falls over on anything that depends on JavaScript or flexbox. The [roadmap](#whats-missing) below is honest about the distance to a real browser.

<p align="center">
  <img src="docs/demo.png" width="32%" alt="The demo page rendered by mybrowser">
  <img src="docs/showcase.png" width="32%" alt="A dark-themed page rendered by mybrowser">
  <img src="docs/tables.png" width="32%" alt="Table layout rendered by mybrowser">
</p>

All three screenshots come from the engine itself. The `--screenshot` flag runs the same layout and paint code as the window, just into an image instead.

## Try it

You need Rust installed. Then:

```sh
cargo run --release                      # opens the demo pages
cargo run --release -- https://example.com
cargo run --release -- some/local/page.html
```

The window has a toolbar with back, forward and reload buttons and an address bar. You can type a full URL, a bare hostname like `example.com/docs`, or a path to a local file. Pages load in the background: the current page stays on screen and responsive until the next one arrives, and `Esc` cancels a load.

| Key | Action |
|---|---|
| `Cmd+L` (or `Ctrl+L`) | Focus the address bar and select it |
| `Enter` / `Esc` | Go to the typed address / cancel editing, or cancel a load in progress |
| `Cmd+V`, `Cmd+C`, `Cmd+A` | Paste, copy and select all in the address bar (macOS) |
| `Cmd+[`, `Cmd+]`, `Alt+←`, `Alt+→`, `Backspace` | Back and forward |
| `Cmd+R`, `F5`, `R` | Reload |
| Wheel, arrow keys, `Space`, `PgUp`, `PgDn`, `Home`, `End` | Scroll |

To render a page to a PNG without opening a window (this works on URLs too):

```sh
cargo run --release -- --screenshot out.png --width 900 --scale 2 https://example.com
```

One heads-up: fonts are loaded from the macOS system font folder (see `src/fonts.rs`), so this only works on a Mac for now. The window code is cross-platform, so on Linux or Windows you'd just need to point the font loader at some TTF files.

## What it can do

**Networking.** It loads `http://` and `https://` pages (TLS through rustls, gzip, redirects, timeouts, a size limit), as well as local files and `data:` URLs. Stylesheets and images are fetched in parallel by a small worker pool with a deadline. A session **cookie jar** is shared by every request: cookies are scoped by host and path, sent back on later requests and across redirects, expire, and are never written to disk. It honours `<base href>`, decodes UTF-8, UTF-16, Latin-1 and Windows-1252 text, and shows readable pages for plain text, images and errors (including HTTP 404s with their bodies). A web page can't load local files or send you to one.

**Browsing.** Pages load on a background thread, so the window never freezes; only the newest navigation can land, so a slow load can't replace a page you've moved on from. Back and forward history changes only when a navigation actually lands, so cancelling or superseding a load never loses or duplicates entries, and typing the address you're already on reloads instead of adding an entry.

**HTML.** Entities, void tags, messy markup (unclosed `<p>` and `<li>`, misnested tags, stray end tags, implied table rows and cells), and a synthesized `<head>` and `<body>` when a page leaves them out. The parser runs in linear time, even on hostile input.

**CSS.** `<style>` blocks, linked stylesheets and inline styles. Tag, class, ID, `*`, descendant and child selectors. Specificity, source order, `!important`, origin (author rules beat the built-in stylesheet), inheritance, and `inherit`/`initial`/`unset`. Most of the box model: width, max-width, height, padding, borders, auto margins, `box-sizing`, and margin collapsing including the awkward cases. Colors in hex, `rgb()`, `hsl()` and by name; the `font` and `border` shorthands.

**Layout.** Block and inline flow, lists, `<pre>`, and **tables**: column widths from min and max content, percentage and fixed widths, `colspan` and `rowspan`, captions, row and cell backgrounds that stretch to the row, vertical alignment, nested tables, and the legacy attributes older pages rely on (`border`, `cellpadding`, `cellspacing`, `bgcolor`, `align`, `valign`, `<font>`).

**Text and images.** Text wraps using real glyph widths with alignment, bold, italic, monospace, underlines, highlights and clickable links. Characters missing from the main fonts (arrows, CJK) fall back to a wide-coverage font. PNG images flow inline with the text, scaled with bilinear filtering, with alt text as the fallback.

## How it works

The engine is a pipeline, and each stage only looks at the output of the one before it:

```
URL -> fetch -> HTML -> DOM --\
                               +-> styles -> layout -> display list -> pixels -> window
         CSS, images <---------/
```

| Stage | File | Notes |
|---|---|---|
| Loading | `src/net.rs` | `file:`, `http(s):` and `data:` URLs, redirects, cookies, limits, content sniffing, charset decoding. |
| HTML to DOM | `src/html.rs`, `src/dom.rs` | One pass over the text with a stack of open elements. The DOM is a flat `Vec` of nodes that refer to each other by index, which keeps Rust's ownership rules out of the way. |
| CSS parsing | `src/css.rs` | Parses rules, matches selectors right to left (with memoization so deep trees stay fast), and expands shorthands like `margin: 0 auto` into their individual properties up front. |
| Styles | `src/style.rs` | Walks the DOM once, collects every matching declaration, sorts by importance, origin, specificity and order, and fills in a typed `Style` for each node. Legacy presentational attributes act as very low-priority rules. |
| Layout | `src/layout.rs` | Stacks blocks, collapses margins, breaks text into lines, sizes and positions table cells. The output is a flat list of "draw this rect" and "draw this text" commands. |
| Painting | `src/paint.rs`, `src/fonts.rs`, `src/images.rs` | Fills rectangles into a `u32` buffer, blends glyph bitmaps and scaled images on top. |
| Browser | `src/page.rs`, `src/chrome.rs`, `src/main.rs` | A page ties the stages together; the chrome is the toolbar and address bar; `main` runs the window, input, links, history and the loader threads. |

A few choices worth explaining:

- Layout produces drawing commands instead of a tree of boxes. Painting is then just "run through the list in order", and scrolling only repaints with an offset. Layout reruns only when the window width changes.
- Selectors I don't support (`a:hover`, `[href]`, `+`) get dropped rather than approximated. Ignoring a rule is better than applying `a:hover` styles to every link.
- Invalid CSS values are ignored instead of resetting the property, so the common "fallback, then modern value" pattern works.
- Table cells are laid out once and then stretched: a row's height isn't known until every cell is done, so backgrounds and borders are patched afterwards instead of laying each cell out twice (which would be exponential for nested tables).
- A page is built whole on a loader thread and handed to the UI thread in one piece. It contains `Rc`s (not `Send`), so the handoff uses a tiny `unsafe impl Send` whose safety argument (every clone lives inside the one value being moved) is written next to it and was reviewed independently.
- The address bar logic (editing, caret, hit-testing) and the navigation and history bookkeeping are plain state with no windowing in them, so they're unit-tested.

## Dependencies

The engine itself is hand-written. I only used crates for the parts that talk to the operating system or are a project of their own:

- `winit` for the window and input events
- `softbuffer` to put a pixel buffer on screen
- `fontdue` to turn a TrueType glyph into a bitmap and report its metrics
- `ureq` (with rustls and its cookie jar) for HTTP and TLS, and `url` for parsing and resolving URLs
- `png` to decode PNG images and save screenshots

## Tests

```sh
cargo test
```

There are well over a hundred tests. They cover the parsers, selector matching, the cascade, layout and tables, address editing, URL handling, history, and networking including cookies (against throwaway local HTTP servers, so no internet is needed). Many are regression tests for real bugs found by throwing nasty input at the engine and by independent review passes: quadratic-time parsing, exponential selector matching, crashes on odd Unicode and huge values, unbounded memory use, history desyncs, and so on. The tests load system fonts, so they also need macOS for now.

## What's missing

The honest list, roughly by how much it matters for a real browser:

- **JavaScript.** None. Pages that build their content with scripts will be blank or broken.
- **Layout.** No flexbox, grid, floats or positioning, no inline padding or margins, and no `vh`/`vw`/`calc()`. A lot of the modern web depends on these.
- **Selectors.** No pseudo-classes (even `:link` and `:first-child`), attribute selectors or `@media`, so many stylesheets lose rules.
- **Browser features.** No tabs, forms or input controls, caching, bookmarks, find-in-page or persistent cookies. Cookies ignore `SameSite` and `Secure` for now.
- **Media.** PNG only: no JPEG, GIF, WebP or SVG. No fonts from the web (`@font-face`).
- **Text.** No shaping, so no kerning, ligatures or complex scripts, and no right-to-left text.
- **Platform.** macOS fonts only, for now.

The order I'm tackling things in: forms and tabs, the CSS gaps above (pseudo-classes, inline boxes, viewport units), a flexbox subset, more image formats, and eventually a JavaScript engine. Until then it's best at documents, blogs, articles and older table-based sites.

## Where to learn more

If you want to go deeper, these are all good:

- Matt Brubeck's [Let's build a browser engine](https://limpet.net/mbrubeck/2014/08/08/toy-layout-engine-1.html) series
- [Web Browser Engineering](https://browser.engineering/) by Pavel Panchekha and Chris Harrelson
- The [CSS 2.1 spec](https://www.w3.org/TR/CSS21/), especially the sections on visual formatting and margin collapsing
