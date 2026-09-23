# winshot — design

Everything measured below was measured on the target machine (WSL2 on Windows, dual
display, both at 200% scaling) on 2026-09-23.

---

## 1. Architecture

WSL2 runs in a separate VM with no access to the Windows desktop compositor. **No Linux
binary can capture the Windows screen.** This is not a library gap; there is no channel.
So the capture process must be a Windows executable, in any language.

```
WSL side                         Windows side
────────                         ────────────
winshot (shell shim)  ──exec──>  winshot.exe
  translate paths                  SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)
  wslpath -w                       enumerate / capture (WGC)
  <──── stdout: final path         encode PNG
                                   write to Windows-native temp
  cp to WSL destination  <──────   (avoid encoding straight onto /mnt/c)
```

**Path handling.** Write the encoded file to Windows-native storage first, then copy to
the WSL destination once. `/mnt/c` goes through the 9p filesystem, which is slow for
many small writes; a single sequential copy of a finished 2-5 MB file is fine (~50 ms).

**Why not keep PowerShell?** Process startup alone is 400-900 ms, before any work. That
is 3-10x the entire budget of the Rust design.

---

## 2. Is Rust actually justified?

Partly. Be honest about where the win comes from — it is **architectural, not
linguistic**. A C++ or C# AOT binary would land in the same place.

Measured / estimated budget at 6912x4338:

| stage | v0 (PowerShell + GDI) | Rust + WGC |
|---|---|---|
| process startup | 400-900 ms | ~5 ms |
| capture | 100-300 ms (GDI BitBlt) | 5-20 ms (GPU, WGC) |
| encode PNG | 400-1200 ms (System.Drawing) | 80-300 ms (fpnge / parallel zune) |
| copy to WSL | ~50 ms | ~50 ms |
| **total** | **~1.5-2.5 s** | **~150-400 ms** |

Rust earns its place on: no runtime startup, direct WinRT/Win32 access with no marshalling,
`rayon` for parallel row-band encoding, and a single static binary with no .NET dependency.

**The encode dominates the Rust budget.** Optimisation effort belongs there, not in the
capture path. `fpnge`-class encoders exceed 1 GB/s; the naive `image` crate PNG encoder is
roughly an order of magnitude slower. Benchmark before choosing.

---

## 3. Capture API

| API | per-monitor | per-window | occluded windows | GPU-composited | notes |
|---|---|---|---|---|---|
| **Windows.Graphics.Capture** | yes | yes | yes | yes | Win10 1903+; the right default |
| DXGI Desktop Duplication | yes | **no** | n/a | yes | fastest for monitors, but no window capture |
| GDI `BitBlt` | yes | partial | no | **no** | misses hardware overlays; what v0 uses |
| GDI `PrintWindow` | no | yes | yes | unreliable | useful fallback only |

**Decision: WGC primary, `PrintWindow` fallback.** WGC is the only API satisfying both
"capture one monitor" and "capture one window" requirements from a single code path.

Gotchas:
- WGC draws a yellow capture border by default. On Win11 set
  `GraphicsCaptureSession.IsBorderRequired = false` (needs a capability declaration on
  some builds). On Win10 it may be unavoidable — document rather than hide it.
- WGC is async/frame-pool based. For a one-shot screenshot, request a frame, wait for
  the first arrival with a timeout, then tear down. Do not leak the session.
- Protected content (DRM video) returns black frames. Detect and warn rather than saving
  a black PNG silently.

### DPI awareness — mandatory

Declare `PER_MONITOR_AWARE_V2` **before** any window or display call, via the app
manifest, not a runtime call (runtime calls race with library init). Without it, Windows
reports and delivers logical pixels and silently downsamples. This is the exact v0 bug:
6912x4338 arrives as 3456x2169 with no error.

### HDR

On an HDR display WGC returns `DXGI_FORMAT_R16G16B16A16_FLOAT` in scRGB. Naively clamping
to 8-bit sRGB produces washed-out, grey-looking screenshots. Detect the format and apply
a tone-map (a simple Reinhard or the SDR white-level from
`DISPLAYCONFIG_SDR_WHITE_LEVEL` is enough). **This is the most likely source of "the
colours look wrong" bug reports.**

---

## 4. Output format

| format | lossless | text quality | size | ecosystem |
|---|---|---|---|---|
| **PNG** | yes | perfect | baseline | universal |
| WebP (lossless) | yes | perfect | ~0.65-0.75x | very good |
| AVIF | optional | good | ~0.5x | slow encode, patchy |
| JPEG | no | **bad** | ~0.3x | universal |
| JPEG XL | yes | perfect | ~0.5x | near-zero support |
| QOI | yes | perfect | ~1.2x | niche |

**JPEG is disqualified.** Its 8x8 DCT rings around high-contrast glyph edges — precisely
the content screenshots consist of. Never make it the default, even behind a size flag.

**Default: PNG.** It is lossless and it is the only format guaranteed to paste into
GitHub issues, Slack, Claude, and Office. `--format webp` uses WebP **lossless** mode
(not lossy) for ~25-35% smaller output when the destination is known to handle it.

### The free lunch: palette reduction

UI and terminal screenshots frequently contain **fewer than 256 unique colours**. When
they do, encoding as an 8-bit indexed PNG is **bit-exact lossless** and typically
**50-75% smaller** than truecolor. This should be the default path, tried automatically:

```
count unique colours (hash set, early-exit past 256)
  <= 256  -> indexed PNG, palette sorted by frequency
  >  256  -> truecolor PNG, per-row adaptive filtering
```

This is where the default size reduction comes from — no resampling, no risk. Measure the
early-exit cost; a full-frame scan at 6912x4338 is ~30 M pixels and must be SIMD or
sampled-then-verified to stay cheap.

**Interaction with downscaling:** resampling *creates* intermediate colours, so a
downscaled image usually exceeds 256 colours and loses the palette win. Halving linear
size cuts pixels 4x but may cost the 2-4x palette advantage — the net gain is far smaller
than naive arithmetic suggests. Always measure both paths before claiming a size win.

---

## 5. The goldilocks downscale

### What actually constrains readability

Not image dimensions — **glyph height in pixels**. Antialiased Latin text stays
comfortably readable down to roughly **7-8 px x-height** and becomes unreliable below
~6 px. So the safe scale is:

```
scale_max = x_height_px_target / x_height_px_measured
```

A fixed ratio is wrong. 0.5x on a 200%-scaled 4K panel is fine; 0.5x on a 1080p display
rendering 12 px text is not.

### Why not SSIM / PSNR

They measure **average pixel fidelity**, which correlates poorly with whether a glyph is
still distinguishable. A blur that renders 6 px text unreadable can still score well
because the surrounding flat UI regions dominate the average. **Readability is a task
metric; measure the task.** Use OCR agreement.

### Three signals, cheapest first

**1. DPI-derived (free, default for `--scale auto`).**
`scale = 1 / dpi_scale_factor`. At 200% this is 0.5, which reproduces what a 100% display
renders. Empirically validated here: a 0.5-scale capture of a dense terminal was fully
legible. Cheap, robust, needs no image analysis.

**2. Measured glyph height (no extra dependencies).**
- Sobel or first-difference edge map
- Sum edge magnitude per row -> 1-D projection
- Text regions appear as periodic high-energy bands; autocorrelation of the projection
  gives line pitch
- x-height ~= 0.5 x line pitch for typical UI fonts
- `scale = 7.5 / x_height`, clamped to [0.25, 1.0]

Fails gracefully on images with no text — detect low periodicity and fall back to signal 1.

**3. OCR-calibrated ground truth (`winshot calibrate`).**
Run once per display, cache in config.

```
reference = OCR(image at 1.0)
binary search s in [0.25, 1.0]:
    candidate = OCR(downscale(image, s))
    agreement = 1 - normalized_levenshtein(candidate, reference)
    accept if agreement >= 0.995
s_safe = accepted_s * 1.1   (safety margin, clamped to <= 1.0)
```

Use a text-dense capture as the calibration target — a terminal or an IDE, not a desktop
wallpaper. Store per-display, since displays differ in DPI and physical size.

### Resampling filter

Filter choice matters as much as the ratio:

| filter | verdict |
|---|---|
| nearest | destroys text; never |
| bilinear | mushy at <0.75 |
| **Lanczos3** | sharp, slight ringing; good default |
| Mitchell | softer, less ringing; better below 0.5 |
| area / box | good for exact integer ratios like 0.5 |

**Default Lanczos3**, switch to **area averaging for exact 1/2 and 1/4** where it is both
faster and artifact-free. Expose `--filter`.

### Default

**Resampling off by default.** The lossless palette path already captures most of the
size win at zero risk, and silently degrading a screenshot the user may be about to read
small text in is the wrong default. `--scale auto` is one flag away, and `calibrate`
writes a per-display value that `auto` then uses.

---

## 6. Flash and shutter

Both **off by default**, both fired **after** the frame is captured — a flash rendered
before capture lands in the screenshot.

**Flash.** Layered, borderless, topmost, click-through window
(`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`) sized to the
captured monitor, filled white, alpha ramped 180 -> 0 over ~120 ms via
`SetLayeredWindowAttributes`. Must never take focus. For `--all`, flash each display.

**Shutter.** Play asynchronously so it never blocks the write path.

> Ship an **originally synthesized** shutter sound. Do not extract, bundle, or reproduce
> the sound asset from iOS or any other product — that file is copyrighted. Synthesize a
> comparable two-transient mechanical click:
> - transient A (mirror up): 3-5 ms filtered noise burst, fast exponential decay
> - short gap, ~60-80 ms
> - transient B (mirror down): slightly lower-amplitude, lower-centroid burst
> - total ~120-150 ms, band-limited around 2-6 kHz
>
> Generate it at build time into `assets/shutter.wav` with a small script so the
> synthesis parameters stay in the repo and the output is reproducible.

---

## 7. Crates

| need | crate | note |
|---|---|---|
| Win32 / WinRT | `windows` | official Microsoft bindings |
| WGC wrapper | `windows-capture` | evaluate; may save meaningful work |
| CLI | `clap` (derive) | |
| PNG encode | benchmark `fpnge` vs `zune-png` vs `image` | encode dominates the budget |
| WebP | `webp` / `libwebp-sys` | lossless mode only |
| resize | `fast_image_resize` | SIMD, has Lanczos3 + area |
| parallelism | `rayon` | row-band encoding |
| config | `serde` + `toml` | per-display calibration cache |
| audio | `rodio`, or raw `PlaySound` | async playback |
| OCR (calibrate only) | `tesseract` binding or shell out | keep behind a `calibrate` feature flag |

Keep OCR **out of the default build** — it is a heavy dependency used by one subcommand.

---

## 8. Open questions for the implementer

1. Does `windows-capture` expose `IsBorderRequired`, or is raw `windows` needed?
2. Measure the real cost of the unique-colour scan at 30 M pixels. If it exceeds ~20 ms,
   sample 1/16 of pixels first and only do the full scan when the sample suggests <256.
3. Is the MSVC target reachable from WSL via `xwin`, or should CI build on
   `windows-latest`? The GNU target cross-compiles more easily but has rougher WinRT
   support — verify before committing.
4. Multi-monitor capture with **mixed DPI** (not the case on this machine, but common):
   does the virtual-desktop capture arrive at a uniform scale, or per-monitor native?
5. Cursor rendering: WGC's `IsCursorCaptureEnabled` vs compositing `GetCursorInfo`
   manually — which gives correct scaling on HiDPI?
