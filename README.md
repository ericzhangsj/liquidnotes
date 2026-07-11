# liquidnotes

Always-on-top sticky notes for Windows with a **real** liquid-glass look — the
notes refract the live desktop behind them, not a flat blur. One small `.exe`,
no dependencies.

## Install

Download `liquidnotes.exe` from the [**Releases**](../../releases) page and run
it (Windows 10 2004+, 64-bit). A glass **+** button appears at the bottom-right
of your screen.

To start it with Windows: right-click the **+** → toggle **Launch on startup**.

## Using it

- **New note** — left-click the **+** (or press **Win+Shift+N**, or left-click
  the tray icon). New notes stack above the button.
- **Move / resize** — drag a note's top strip to move it; pull any edge or
  corner to resize.
- **Delete** — flick a note fast and let go: it spins off the screen. Bring the
  last one back with **Ctrl+Shift+Z**, restored to right where it was.
- **Dock** — drop a note against the left or right screen edge to tuck it away
  as a thin sliver; click it to slide it back.
- **Edit** — click a note's body to type. **Double-click** selects a word,
  **triple-click** a sentence.
- **Settings** — right-click the **+** for a pill menu: **Quit**, **Launch on
  startup**, and **Opacity** / **Size** sliders.

### Keyboard shortcuts

| Keys | Action |
|---|---|
| Double-click / triple-click | Select the word / sentence |
| Arrows | Move the caret (Up/Down across wrapped lines) |
| Ctrl+← / Ctrl+→ | Move by word |
| Home / End | Start / end of the line |
| Ctrl+Home / Ctrl+End | Start / end of the note |
| Shift + any of the above | Extend the selection |
| Ctrl+A | Select all |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste |
| Ctrl+Z / Ctrl+Y | Undo / redo the text edit |
| Ctrl+Shift+Z | Bring back the last deleted note |
| Backspace / Delete | Delete a character |
| Ctrl+Backspace / Ctrl+Delete | Delete a word |
| Ctrl+B / Ctrl+I | Bold / italic the selection |
| Ctrl+S | Strikethrough the selection (or force-save if nothing is selected) |
| Ctrl+= / Ctrl+- | Grow / shrink the note's font |
| Ctrl+W | Close the note |

## Under the hood

You won't really notice any of this — it's just there to look and read nicely:

- **Real liquid glass.** The engine captures the desktop on the GPU and runs a
  refraction shader (curved-surface normals → Snell refraction → frost → rim
  light) per note, so the glass genuinely bends the live background behind it.
- **Crisp text.** Note text is rendered supersampled and averaged back down, so
  glyphs stay sharp on any display.
- **Invisible in captures.** By default notes stay out of screenshots and screen
  recordings (the backdrop keeps playing underneath); a tray toggle switches to
  a mode where they show in captures instead.

## Build from source

```
rustup default stable
cargo build --release
```

Produces a self-contained `target\release\liquidnotes.exe`. Building with the
GNU toolchain instead of MSVC also needs
[w64devkit](https://github.com/skeeto/w64devkit) on PATH (for `dlltool` / `as` /
`windres`).

## License

MIT — see [LICENSE](LICENSE).
