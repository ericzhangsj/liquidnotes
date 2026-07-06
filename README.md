# liquidnotes

Always-on-top sticky notes for Windows with a **real** liquid-glass material —
not the OS acrylic/blur. The engine captures the desktop on the GPU (DXGI
Desktop Duplication), reconstructs a background-only texture, and runs a
single-pass refraction shader (SDF height field → surface normals → Snell
refraction → frost → rim light) composited through DirectComposition. Each note
is its own `WS_EX_NOREDIRECTIONBITMAP` window whose pixels come entirely from
the glass engine.

- **Single portable `.exe`**, no runtime dependencies, Windows 10 2004+ (x64).
- Rich-text notes, edge docking, flick-to-delete, launch-on-startup, tray icon.
- **Zero means zero**: every material knob at `0` renders a pixel-perfect
  passthrough of the sharp desktop (the note becomes optically invisible).

## Install

1. Download `liquidnotes.exe` from the [**Releases**](../../releases) page.
2. Double-click it. There's nothing to install — the notes save themselves to
   `%APPDATA%\liquidnotes\notes.json`, and you can move or delete the `.exe`
   freely.

> **First run:** because the build isn't code-signed, Windows SmartScreen may
> show *"Windows protected your PC."* Click **More info → Run anyway**. This is
> normal for small open-source apps.

To have it start with Windows, right-click the ➕ button → toggle **Launch on
startup** (writes an `HKCU\...\Run` entry; toggle it off to remove).

## Using it

A glass ➕ button sits at the bottom-right of the screen:

- **Left-click ➕** (or press **Win+Shift+N**, or left-click the tray icon) —
  spawn a new note, stacked above the button.
- **Right-click ➕** — a pill menu fans out: **Quit**, and a **Launch on
  startup** toggle.

Notes:

- **Drag** the top strip to move a note; pull any **edge/corner** to resize.
- **Flick** a note fast and let go to **throw it away** (delete) — it spins off
  and can be restored with Ctrl+Z.
- Drop a note against the **left/right screen edge** to **dock** it as a thin
  sliver; hover to peek, click to bring it back.
- Click a note's body to edit; only the focused note shows a caret.

### Keyboard shortcuts (while editing a note)

| Keys | Action |
|---|---|
| Arrows | Move the caret (Up/Down across wrapped lines) |
| Ctrl+← / Ctrl+→ | Move by word |
| Home / End | Start / end of the line |
| Ctrl+Home / Ctrl+End | Start / end of the note |
| Shift + any of the above | Extend the selection |
| Ctrl+A | Select all |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / cut / paste |
| Ctrl+Z / Ctrl+Y (or Ctrl+Shift+Z) | Undo / redo the text edit |
| Backspace / Delete | Delete a character |
| Ctrl+Backspace / Ctrl+Delete | Delete a word |
| Ctrl+B / Ctrl+I | Bold / italic the selection |
| Ctrl+S | Strikethrough the selection (or save now if nothing is selected) |
| Ctrl+= / Ctrl+- | Grow / shrink the note's font |
| Ctrl+W | Close the note |

With no note focused, **Ctrl+Z** restores the most recently deleted note.

### Backdrop modes

**Live** (default) keeps video etc. playing under the glass, but hides notes
from screenshots/recordings. The tray menu's *Live backdrop* toggle switches to
**reconstruction** mode — notes then show in captures, at the cost of content
fully hidden under a stationary note freezing until revealed.

## Building from source

Stock Rust on Windows uses the MSVC toolchain (this is what CI ships):

```
rustup default stable
cargo build --release
```

The result is `target\release\liquidnotes.exe`. Release builds statically link
the C runtime (see `.cargo/config.toml`) so the `.exe` is self-contained.
Tagged pushes (`v*`) build and attach the `.exe` to a GitHub Release
automatically (`.github/workflows/release.yml`).

### Without Visual Studio (GNU toolchain)

To build entirely at the user level, without VS Build Tools:

1. `rustup default stable-x86_64-pc-windows-gnu`
2. Install [w64devkit](https://github.com/skeeto/w64devkit) (portable zip) and
   put its `bin` on PATH — the `windows` crates use raw-dylib linking, which
   needs binutils' `dlltool`/`as` that rustup doesn't bundle.
3. w64devkit ships no `libgcc_eh.a` (its unwinder lives in `libgcc.a`); create
   an empty stub next to its `libgcc.a`:
   `ar rcs lib\gcc\x86_64-w64-mingw32\<ver>\libgcc_eh.a`

## Material tuning

Until a settings UI lands, the glass is tunable live via environment variables
(all default to a tasteful preset; set any to `0` to disable that effect):

| Env var | Effect |
|---|---|
| `LN_REFRACT` | How violently the backdrop warps. `0` = refraction off |
| `LN_DEPTH` | Dome reach, `0`–`1`: higher pushes the curve toward the center |
| `LN_FROST` | Gaussian frost blur radius. `0` = blur pass skipped |
| `LN_CORNER` | Corner radius (px) |
| `LN_BORDER` | Rim/bevel width (px) |
| `LN_BREFRACT` | Extra refraction at the rim |
| `LN_LIGHT` | Rim glint intensity. `0` = off |
| `LN_LANGLE` | Rim glint angle (degrees) |
| `LN_OPACITY` | Adaptive card-fill amount, `0`–`1` (`0` = clear glass) |
| `LN_BACKDROP` | `capture` forces reconstruction mode (notes visible in screenshots) |

Example: `$env:LN_FROST=8; $env:LN_OPACITY=0.3; .\liquidnotes.exe`

## License

MIT — see [LICENSE](LICENSE).
