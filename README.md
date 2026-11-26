# mybrowser

I wanted to understand what a browser actually does between receiving some HTML and putting pixels on a screen, so I wrote the rendering part myself in Rust. It parses HTML and CSS, works out which styles apply to what, lays everything out, and paints it into a pixel buffer. There's no WebKit or Blink underneath, and no UI toolkit doing the text layout for me.

It's about 2,300 lines including tests, so you can read the whole thing in an afternoon. It's nowhere near a real browser, but it handles a surprising amount of everyday HTML and CSS.

<p align="center">
  <img src="docs/demo.png" width="49%" alt="The demo page rendered by mybrowser">
  <img src="docs/showcase.png" width="49%" alt="A dark-themed page rendered by mybrowser">
</p>

Both screenshots come from the engine itself. The `--screenshot` flag runs the same layout and paint code as the window, just into an image instead.

## Try it

You need Rust installed. Then:

```sh
cargo run --release -- pages/index.html
```

That opens a window. Scroll with the wheel or arrow keys, click the links to move between the sample pages, press `Backspace` to go back, and press `R` to reload the file from disk. The reload key is handy while experimenting: edit some HTML or CSS, hit `R`, see the change.

To render a page to a PNG without opening a window:

```sh
cargo run --release -- --screenshot out.png --width 760 --scale 2 pages/index.html
```

One heads-up: fonts are loaded from the macOS system font folder (see `src/fonts.rs`), so this only works on a Mac for now. The window code is cross-platform, so on Linux or Windows you'd just need to point the font loader at some TTF files.

## What it can do

On the HTML side, it handles entities, void tags like `<br>` and `<img>`, messy markup (unclosed `<p>` and `<li>`, misnested tags), and builds the `<head>` and `<body>` for you when a page leaves them out.

For CSS, it reads `<style>` blocks, linked stylesheets and inline `style` attributes. Selectors can be tags, classes, IDs, `*`, and the descendant and child combinators. Specificity, source order, `!important` and inheritance all work. Most of the box model is there too: width, max-width, height, padding, borders, auto margins for centering, `box-sizing`, and margin collapsing, including the awkward cases like a parent and its first child, or negative margins.

Text is wrapped using real glyph widths, with alignment, bold and italic, monospace, underlines, highlighted spans and clickable links. Lists and `<pre>` blocks work as well, and local PNG images flow inline with the text (scaled with bilinear filtering, with alt text as the fallback).

## How it works

The engine is a pipeline, and each stage only looks at the output of the one before it:

```
HTML  -> DOM  --\
                 +-> styles -> layout -> display list -> pixels -> window
CSS   -> rules -/
```

| Stage | File | Notes |
|---|---|---|
| HTML to DOM | `src/html.rs`, `src/dom.rs` | One pass over the text with a stack of open elements. The DOM is a flat `Vec` of nodes that refer to each other by index, which keeps Rust's ownership rules out of the way. |
| CSS parsing | `src/css.rs` | Parses rules, matches selectors right to left, and expands shorthands like `margin: 0 auto` into their individual properties up front. |
| Styles | `src/style.rs` | Walks the DOM once, collects every matching declaration, sorts by importance, specificity and order, and fills in a typed `Style` for each node. |
| Layout | `src/layout.rs` | Stacks blocks, collapses margins, breaks text into lines. The output is a flat list of "draw this rect" and "draw this text" commands. |
| Painting | `src/paint.rs`, `src/fonts.rs` | Fills rectangles into a `u32` buffer and blends glyph bitmaps on top. |
| Window | `src/main.rs`, `src/page.rs` | Opens the window and handles scrolling, links and history. |

A few choices worth explaining:

- Layout produces drawing commands instead of a tree of boxes. Painting is then just "run through the list in order", and scrolling only repaints with an offset. Layout reruns only when the window width changes.
- Selectors I don't support (`a:hover`, `[href]`, `+`) get dropped rather than approximated. Ignoring a rule is better than applying `a:hover` styles to every link.
- Text is positioned by its baseline, so different font sizes on the same line line up properly.

## Dependencies

The engine itself is hand-written. I only used crates for the parts that talk to the operating system or are a project of their own:

- `winit` for the window and input events
- `softbuffer` to put a pixel buffer on screen
- `fontdue` to turn a TrueType glyph into a bitmap and report its metrics
- `png` to save screenshots

## Tests

```sh
cargo test
```

There are 41 tests covering the parsers, selector matching, the cascade and layout. A few of them are regression tests for real bugs I found by throwing nasty input at the engine: text starting with a non-ASCII character crashing the tokenizer, a huge `font-size` killing the process, negative widths, an unclosed `<head>` hiding the whole page, and absurdly deep nesting. The tests load system fonts, so they also need macOS for now.

## What's missing

Quite a lot. There are no JPEG/GIF/SVG images (PNG only), tables, flexbox, grid, floats or positioning. No pseudo-classes, attribute selectors, `@media`, `var()` or `calc()`. Text goes through no shaping, so no kerning or ligatures and no complex scripts. Pages load from local files only, with no `http://` yet, and there's no JavaScript.

What I'd like to do next, roughly in this order: loading pages over HTTP, table layout, a flexbox subset, an address bar, and eventually replacing `fontdue` with my own glyph rasterizer.

## Where to learn more

If you want to go deeper, these are all good:

- Matt Brubeck's [Let's build a browser engine](https://limpet.net/mbrubeck/2014/08/08/toy-layout-engine-1.html) series
- [Web Browser Engineering](https://browser.engineering/) by Pavel Panchekha and Chris Harrelson
- The [CSS 2.1 spec](https://www.w3.org/TR/CSS21/), especially the sections on visual formatting and margin collapsing
