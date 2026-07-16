# liquidnotes

Always-on-top sticky notes for Windows with a **real** liquid-glass look and a
same-composition-frame live backdrop. One small `.exe`, no dependencies.

## Install

Download the executable for your PC from the [**Releases**](../../releases)
page: `liquidnotes-x64.exe` for Intel/AMD or `liquidnotes-arm64.exe` for a
Windows-on-ARM laptop. Run it on Windows 10 2004 or newer; a glass **+** button
appears at the bottom-right of your screen.

To start it with Windows: right-click the **+** → toggle **Launch on startup**.

## Using it

- **New note** — left-click the **+** (or press **Win+Shift+N**, or left-click
  the tray icon). New notes stack above the button.
- **Move / resize** — hover a note to reveal its top handlebar, drag that handle
  to move it, or pull any edge or corner to resize.
- **Delete** — flick a note fast and let go: it spins off the screen. Bring the
  last one back with **Ctrl+Shift+Z**, restored to right where it was.
- **Dock** — bring the cursor near the left or right screen edge while dragging
  to tuck a note away as a thin sliver. The optional **Slide out hidden notes**
  setting reveals it on direct hover; revealed notes can still be edited and
  resized without losing their border position.
- **Edit** — click a note's body to type. **Double-click** selects a word,
  **triple-click** a sentence.
- **Settings** — right-click the **+** for a pill menu: **Quit**, **Launch on
  startup**, **Slide out hidden notes**, and **Opacity** / **Size** sliders.

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

- **Same-frame glass.** By default DWM supplies each note's blurred backdrop in
  the composition pass that draws the desktop, while LiquidNotes adds its tint,
  curved-surface rim, glow, and text as a transparent GPU overlay. Scrolling
  therefore cannot wait on a capture/readback/app-present round trip.
- **Exact fallback.** `LN_RENDERER=capture` selects the original GPU desktop-
  duplication renderer with curved-surface refraction, frost, and rim lighting.
  Its frame queue is capped at one and its luminance readback is asynchronous.
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

Official releases are built and tested natively for both
`x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc`.

## License

MIT — see [LICENSE](LICENSE).
