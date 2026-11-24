# canvas-render

HTML to PNG plus a layout map of `data-cv-id` elements, on CPU, with no browser. Built on Blitz 0.3.0-beta.4 (`blitz-html`, `blitz-dom`, `blitz-paint`) and `anyrender_vello_cpu`. Wired into `snippet` via `src/canvas/render.rs`.

```rust
let r = canvas_render::render(html, 800, 0, 2.0)?;   // width, height (0 = content, max 8000), scale 1..3
r.png;      // RGBA PNG, r.width x r.height device px
r.layout;   // Vec<ElementRect>: data-cv-id, border box in logical frame-local px, document order
canvas_render::hit_test(html, 800, x, y);       // deepest data-cv-id at a logical point
```

CLI: `cargo run --release --example render -- in.html out.png [width] [scale]` writes the PNG and prints `{width, height, layout}` JSON. `cargo run --release --example bench` measures the three test frames.

## Behaviour

- Fonts are bundled and system fonts are off, so output is deterministic. Generics map to Inter (sans, default), DejaVu Serif, JetBrains Mono and Twemoji. See `FONTS.md`.
- No network. Only `data:` URIs are loaded (images, stylesheets, fonts); every `http(s)` or `file` resource is skipped silently.
- Width above 4000 or height above 8000 logical px returns `RenderError::TooLarge`. With height 0 the frame is the content height, capped at 8000; `vh` units resolve against an 800 px viewport in that mode.
- Inline elements (`span`, `code`, `a`, ...) get the union of their line boxes as their rect, since Blitz gives them no box of their own.
- Paint panics are caught and returned as `RenderError::Paint`.

## Performance

Release build, warm, 800 px wide at 2x, three test frames (flex cards, grid dashboard, text with radii and shadows), 150 renders, full parse + style + layout + paint + PNG encode, 8-core x86_64:

- p50 6.3 ms, p95 14.9 ms (largest-shadow frame dominates the tail; cold first render 10 to 30 ms).
- `examples/render` binary: 27.0 MB, 22.4 MB stripped (includes 4.5 MB of fonts), default release profile.

## Known gaps

These render wrongly or not at all in Blitz 0.3.0-beta.4. Agent-written HTML should avoid them.

- `<select>` renders as an empty box; `<input placeholder>` text is not drawn. Use styled `div`s for form mock-ups.
- `text-overflow: ellipsis` is ignored; overflowing text is hard clipped.
- `background-clip: text` (gradient text) paints a solid gradient box.
- `filter` (e.g. `blur()`) is ignored. Prefer plain colours and `box-shadow`.
- A spread-only `box-shadow` (rings) on `border-radius: 50%` comes out as a rounded square, not a circle.
- Page CSS does not reach inline SVG: `svg text { fill; font-family }` from a stylesheet is ignored and SVG text falls back to serif. Put presentation attributes on SVG elements directly.
- Body or root background does not extend past the body height (white strip below short content on tall frames); set the background on a full-height wrapper.
- `ol` markers sit flush against the text (no gap); nested list indents differ slightly.
- `border-collapse` tables: cell padding and row borders differ slightly from browsers.
- Italic serif text renders upright (no serif italic bundled, no synthetic oblique). Sans and mono italics are real.
- No CJK, Arabic, Hebrew, Devanagari or other non-Latin/Greek/Cyrillic scripts: they draw as missing-glyph boxes. Emoji work (Twemoji).
- `font-variation-settings` and `font-feature-settings` are unsupported; use `font-weight`.
- No CSS animations or transitions, no JavaScript, no `:hover` states; each render is a static frame.
- External images, stylesheets and web fonts are never fetched; inline them as `data:` URIs.
