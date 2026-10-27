# Inline image previews

xi shows PNG screenshots returned as tool image blocks, including `read`, and
images in submitted messages. Previews are at most 48 columns by 12 rows and
include the source dimensions. They appear in inline scrollback and the deck.
The draft composer still shows an attachment count, not thumbnails.

Graphics are detected automatically for Kitty and Ghostty (Kitty graphics), and
iTerm2 and WezTerm (OSC 1337). Unknown terminals and sessions inside tmux or
screen keep the existing text summaries. Merely printing a file path from a
shell command does not produce a preview; ask the agent to read the PNG.

To try a local build:

```sh
cargo run -p xi-cli --
```

Ask xi to read a screenshot, or paste an image with the configured `paste_image`
key and submit it. To override detection, set `XI_IMAGES` before starting xi:

| Value | Behavior |
| --- | --- |
| `auto` or unset | Detect a known terminal, without multiplexer passthrough. |
| `kitty` | Emit Kitty graphics directly. |
| `iterm2` | Emit OSC 1337 graphics directly. |
| `off` | Keep text summaries only. |

For example, `XI_IMAGES=kitty cargo run -p xi-cli --` forces Kitty graphics.
Overrides do not implement tmux/screen passthrough; test in a direct terminal
first. Color settings do not disable images; use `XI_IMAGES=off` for that.

## Limits

This first implementation previews PNG only. Invalid data, other formats,
encoded payloads over 8 MiB, dimensions over 16,384 pixels, and images exceeding
the decoder's 64 MiB allocation limit fall back to a text notice. The original
attachment sent to the model is unchanged.

Images are decoded and re-encoded as small PNGs before reaching the terminal.
Tool text cannot inject graphics escape sequences. The screen reserves rows
before drawing, and does not draw a preview clipped by a deck viewport or a
narrower terminal. Deck placements are removed before redraw and on exit.
Scrollback retention and resize behavior also depend on the terminal's graphics
implementation. Preview sizing assumes roughly 2:1 cell height to width.
