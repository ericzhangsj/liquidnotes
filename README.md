# liquidnotes

Sticky notes for Windows with a **real** liquid-glass material — not an OS
acrylic/blur. The engine captures the desktop on the GPU (DXGI Desktop
Duplication), reconstructs a background-only texture, and runs a single-pass
physics shader: SDF height field → surface normals → Snell refraction →
Cauchy chromatic dispersion → Blinn-Phong specular → rim meniscus.

- Single portable `.exe`, no runtime dependencies (Windows 10 1903+ x64).
- Every material parameter is live-tunable, and **zero means zero**:
  all sliders at 0 = pixel-perfect passthrough of the sharp desktop.

## Material parameters

| Parameter | Effect |
|---|---|
| `MATERIAL_REFRACTIVE_INDEX` | How violently the backdrop warps. `0.0` = refraction off |
| `SURFACE_TENSION_FALLOFF` | Width of the curved meniscus edge band |
| `CHROMATIC_DISPERSION_AMOUNT` | R/G/B wavelength separation (prism fringe). `0.0` = none |
| `FROST_BLUR_RADIUS` | Gaussian pre-blur under the physics. `0` = pass skipped entirely |

## Build

```
rustup default stable
cargo build --release
```

Releases are built automatically by GitHub Actions on tagged pushes.
