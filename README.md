# liquidnotes

Sticky notes for Windows with a **real** liquid-glass material — not an OS
acrylic/blur. The engine captures the desktop on the GPU (DXGI Desktop
Duplication), reconstructs a background-only texture, and runs a single-pass
physics shader: SDF height field → surface normals → Snell refraction →
Cauchy chromatic dispersion → Blinn-Phong specular → rim meniscus.

- Single portable `.exe`, no runtime dependencies (Windows 10 1903+ x64).
- Every material parameter is live-tunable, and **zero means zero**:
  all sliders at 0 = pixel-perfect passthrough of the sharp desktop.

## Use

Run `liquidnotes.exe`. A glass ➕ button docks at the bottom-right of the
screen: **left-click** spawns a note stacked above it (drag anywhere to move,
pull any edge/corner to resize), **right-click** opens the menu (New note / backdrop mode / Quit).

Backdrop modes: **live** (default) keeps video etc. playing under the glass
but hides notes from screenshots/recordings; unchecking it switches to
reconstruction mode — notes show in captures, at the cost of content that is
fully hidden under a stationary note freezing until revealed.

## Material parameters

| Parameter | Effect |
|---|---|
| `MATERIAL_REFRACTIVE_INDEX` | How violently the backdrop warps. `0.0` = refraction off |
| `SURFACE_TENSION_FALLOFF` | Dome restriction: lower bleeds the curve deeper into the center (1.0 = reaches center), higher confines it to the border. `0` = flat |
| `CHROMATIC_DISPERSION_AMOUNT` | R/G/B wavelength separation (prism fringe). `0.0` = none |
| `FROST_BLUR_RADIUS` | Gaussian pre-blur under the physics. `0` = pass skipped entirely |

Quick experiments via env vars (until the settings UI lands):
`LN_REFRACT`, `LN_TENSION`, `LN_DISPERSION`, `LN_FROST`, `LN_HEIGHT`,
`LN_SPEC`, `LN_RIM`, `LN_TINT` — e.g. `$env:LN_FROST=8; .\liquidnotes.exe`.
Setting all of them to 0 renders a pixel-perfect passthrough (verified by
screenshot diff — the note becomes optically invisible).

## Build

```
rustup default stable
cargo build --release
```

Releases are built automatically by GitHub Actions on tagged pushes.

### Building on Windows without Visual Studio (GNU toolchain)

The MSVC toolchain (default, used by CI) needs VS Build Tools. To build with
the fully user-level GNU toolchain instead:

1. `rustup default stable-x86_64-pc-windows-gnu`
2. Install [w64devkit](https://github.com/skeeto/w64devkit) (portable zip)
   and put its `bin` on PATH — the `windows` crates use raw-dylib linking,
   which needs binutils' `dlltool`/`as` that rustup does not bundle.
3. w64devkit ships no `libgcc_eh.a` (its unwinder lives in `libgcc.a`);
   create an empty stub next to its `libgcc.a`:
   `ar rcs lib\gcc\x86_64-w64-mingw32\<ver>\libgcc_eh.a`
