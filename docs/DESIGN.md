# winshot design

Measurements below were taken on the target machine (WSL2 on Windows 11 build 26200,
two displays at **mixed DPI**: DISPLAY1 3840x2400 at 200%, DISPLAY5 1920x1080 at 125%)
on 2026-09-23. Where something is an estimate or unverified, it says so.

---

## 1. Architecture

WSL2 runs in a separate VM with no access to the Windows desktop compositor. **No Linux
binary can capture the Windows screen.** This is not a library gap; there is no channel.
So the capture process must be a Windows executable, in any language.

```
WSL side                         Windows side
--------                         ------------
winshot (bash shim)   --exec-->  winshot.exe  (manifest: PER_MONITOR_AWARE_V2)
  absolute WSL paths               assert DPI awareness
  -> wslpath -w                    enumerate / capture (WGC)
                                   encode PNG in memory
  <-- "<path> (WxH)"               one write_all to the destination
  Windows path -> wslpath -u
```

**Path handling.** A Windows process launched from WSL inherits the
`\\wsl.localhost\<distro>\...` working directory, so relative output paths work unchanged;
the shim only translates absolute paths. The file is encoded fully in memory and written
with a single sequential write: 5-25 ms measured onto a `\\wsl.localhost` path for
0.1-0.8 MB files. The originally planned "write to Windows temp, then copy" step was
unnecessary and was dropped.

**Build.** The Windows-native toolchain (`cargo.exe`, `x86_64-pc-windows-msvc`, VS Build
Tools) builds straight from the WSL source tree with a target dir on NTFS (`build.sh`).
No cross-compilation, no `xwin`.

---

## 2. Is Rust actually justified?

Partly. The win is **architectural, not linguistic**: a C++ or C# AOT binary would land
in the same place. And the speed win turned out smaller than estimated.

Whole virtual desktop, 5760x2711, wall time launched from WSL (3 runs each):

| | resolution | wall time |
|---|---|---|
| v0, original (DPI-unaware) | 3456x2169 | 515-546 ms (one outlier at 3.2 s) |
| v0, DPI-fixed | 5760x2711 | 641-671 ms |
| winshot | 5760x2711 | 413-443 ms |

Inside winshot (`--verbose`), per run:

| stage | measured |
|---|---|
| process start + WSL interop | ~50-80 ms (wall minus internal total) |
| D3D11 device | 36-53 ms |
| WGC capture, one display, incl. device | 181-248 ms |
| WGC capture, both displays | 243-287 ms |
| PNG encode, 15.6 Mpx, balanced | ~100 ms |
| write onto `\\wsl.localhost` | 5-25 ms |

The original estimates (v0 1.5-2.5 s, winshot 150-400 ms, "encode dominates") were wrong
on two counts: v0 was never that slow on this machine, and **capture, not encoding,
dominates** winshot's budget. The per-grab cost is mostly WGC session setup and waiting
for the first frame. Starting the per-monitor sessions concurrently is the obvious next
optimisation, worth roughly 50-100 ms on two displays (estimate, not measured).

---

## 3. Capture API

| API | per-monitor | per-window | occluded windows | GPU-composited | notes |
|---|---|---|---|---|---|
| **Windows.Graphics.Capture** | yes | yes | yes | yes | Win10 1903+; used |
| DXGI Desktop Duplication | yes | **no** | n/a | yes | no window capture |
| GDI `BitBlt` | yes | partial | no | **no** | what v0 uses |
| GDI `PrintWindow` | no | yes | yes | unreliable | not implemented, see below |

**Decision: WGC only**, through the raw `windows` crate (`src/capture.rs`, ~360 lines incl. enumeration; the
`windows-capture` wrapper was not needed). One-shot flow: free-threaded frame pool, one
frame via `FrameArrived` with a 2 s timeout, copy to a staging texture, tear down.

- **Capture border.** `IsBorderRequired = false` is accepted on build 26200 for this
  unpackaged exe, and no yellow border appears in captures. On builds that refuse it,
  `--verbose` says so.
- **Blank frames.** A frame that is entirely black triggers a warning; it is never saved
  silently. When several windows match a selector, blank ones are skipped, and so are
  ones that fail to capture at all: CapCut owns two visible, titled, top-level helper
  windows that capture black, stacked above its real window, which captures fine. Only
  when every match fails does the command fail.
- **Minimized windows** cannot be captured by WGC; winshot restores them with
  `SW_SHOWNOACTIVATE` (no focus change), captures, and re-minimizes.
- **`PrintWindow` fallback: not implemented.** No WGC failure was observed on the target.
  If one turns up, the black-frame warning makes it visible, and that is the point to add it.

### DPI awareness

Declared as `PER_MONITOR_AWARE_V2` in the **embedded manifest** (`build.rs`,
`embed-manifest`), not a runtime call. Without it, Windows reports and delivers logical
pixels and silently downsamples: the v0 bug. Because that failure is silent, winshot
checks three times:

1. startup: the thread's DPI context must equal PMv2, else abort;
2. enumeration: each monitor's rect must equal its display mode (`EnumDisplaySettingsW`,
   physical by definition), else abort; if the mode cannot be read at all, also abort;
3. per frame: a monitor frame must be exactly the monitor's physical size, else nothing
   is saved. Window frames are compared with `DWMWA_EXTENDED_FRAME_BOUNDS` and warn on a
   mismatch over 10%. (Maximized windows legitimately come back ~26 px larger: WGC includes
   the off-screen resize border.)

### Mixed DPI (was open question 4)

Under PMv2 the virtual desktop is in physical pixels **per monitor**: DISPLAY5 sits at
(-1920, 1631) physical versus (-1536, 1305) as a DPI-unaware process sees it. WGC delivers
each monitor at its native resolution, and winshot composites them at their physical
positions (gaps are black). Verified on this rig, 200% + 125%. This also invalidated the
original handoff's figures: it listed DISPLAY5 as 3072x1728 at 200% and the desktop as
6912x4338, which were logical x2, not measurements.

### Cursor (was open question 5)

WGC's own `IsCursorCaptureEnabled` renders the cursor at the correct HiDPI size: 25x37 px
on the 200% display, about twice the 100% arrow. No manual compositing needed. One
unexplained observation: over WezTerm on DISPLAY5, one `--cursor` capture contained no
cursor while `GetCursorInfo` reported it showing. Likely the app hid the pointer; not
confirmed.

### HDR (unverified)

Neither display here is HDR (`list displays` detects it via `IDXGIOutput6` colour space).
winshot always requests `B8G8R8A8UIntNormalized`, which asks Windows to do the HDR-to-SDR
conversion; `--verbose` flags HDR displays. Whether that conversion looks right has **not
been tested**. It is the most likely source of "colours look wrong" reports.

---

## 4. Output format

**Default: PNG** via the `png` crate, zlib level "balanced". JPEG is not offered at all:
its 8x8 DCT rings around glyph edges. `--format webp` gives WebP **lossless** via the
pure-Rust `image-webp`.

Encoder benchmark on real captures (`winshot --bench-encode`, hidden flag):

| capture | Mpx | fast | balanced | high | webp lossless |
|---|---|---|---|---|---|
| DISPLAY1 | 9.2 | 825 KB / 9 ms | 412 KB / 64 ms | 410 KB / 167 ms | 501 KB / 36 ms |
| whole desktop | 15.6 | 1530 KB / 19 ms | 697 KB / 100 ms | 691 KB / 254 ms | 931 KB / 53 ms |
| WezTerm (terminal) | 2.0 | 476 KB / 3 ms | 173 KB / 17 ms | 172 KB / 51 ms | 280 KB / 6 ms |
| Sublime Text | 2.9 | 525 KB / 4 ms | 244 KB / 22 ms | 243 KB / 59 ms | 419 KB / 9 ms |
| Chrome | 8.8 | 901 KB / 10 ms | 501 KB / 57 ms | 501 KB / 164 ms | 680 KB / 28 ms |
| Claude desktop | 8.8 | 1016 KB / 9 ms | 547 KB / 59 ms | 546 KB / 172 ms | 455 KB / 29 ms |
| Teams | 8.8 | 611 KB / 10 ms | 407 KB / 55 ms | 410 KB / 142 ms | 262 KB / 32 ms |
| Outlook | 8.9 | 1156 KB / 14 ms | 767 KB / 78 ms | 769 KB / 246 ms | 696 KB / 32 ms |

Conclusions:

- **Balanced** is the default: about half the size of fast for ~50 ms per 9 Mpx. High
  buys under 1%. (`fpnge` has no Rust crate; `zune-png` has no encoder; `png`'s fdeflate
  path is already fast enough that a multithreaded encoder was not worth adding.)
- **WebP lossless is not reliably smaller.** It lost to balanced PNG on 5 of 8 captures
  (by up to 70%) and won on 3 (by 9-36%). The original "~25-35% smaller" claim is refuted.

### The palette path is not the free lunch

The design assumed UI screenshots often have <= 256 colours, so indexed PNG would be the
default size win. **Measured: 0 of 8 real captures qualify**, including the terminal:
antialiased and ClearType text alone exceeds 256 colours. The only qualifying frame was a
blank helper window.

The path is kept because it is nearly free: the scan only hashes colour *changes* and
exits at colour 257, costing 0.2-1.8 ms on the captures above. When it applies, it is
bit-exact lossless (unit-tested round-trip). The palette is in first-seen order, not
frequency order; that is a known minor gap with no measured effect. The size win comes
from the zlib level instead.

Resampling creates intermediate colours, so a downscaled image practically never takes
the palette path.

---

## 5. The goldilocks downscale

### What constrains readability

**Glyph height in pixels**, not image dimensions. A fixed ratio is wrong: 0.5x on a
200%-scaled panel is fine, while 0.5x on a 100% display with small text is not.

### Why not SSIM / PSNR

They measure average pixel fidelity. A blur that makes small text unreadable can still
score well, because flat UI regions dominate the average. Readability is a task metric,
so measure the task: OCR.

### What the OCR measurement actually needed (measured)

The original spec was: OCR the image at 1.0 and at candidate scales, take whole-string
normalised Levenshtein agreement, and accept >= 99.5%. On real captures with the Windows
OCR engine, that **never passes**:

- Whole-string agreement never exceeded 90% at any scale, was 69-84% at x0.988 (an
  invisible change), and was not monotone (x0.906 scored below x0.812). A small rescale makes the engine
  re-segment and re-order lines and columns, and edit distance then scores *reading
  order*, not legibility.
- Switching to order-insensitive **word recall** (multiset) fixed the ordering problem
  but still left a noise ceiling of 82-97% at x0.99.
- OCRing the candidate **after scaling it back up** to the reference size (so the engine
  always sees the same geometry, and only the glyph information lost in the downscale
  varies) made the curve close to monotone, with the ceiling at 83-98% (x0.99).

So Windows OCR cannot re-read even an unchanged screen at 99.5%. The implemented
criterion is **relative to the engine's own noise floor**, and it scores **only the
smallest text size** (next section):

```
small   = words on lines of the smallest text size (see below)
floor   = recall(small, OCR(upscale(downscale(img, 0.99))))
accept s  if  recall(small, OCR(upscale(downscale(img, s)))) >= 0.90 * floor
binary search s in [0.25, 0.99], 5 steps; refuse if floor < 50%
```

`RETAIN = 0.90` in `src/calibrate.rs` is the tuning knob; raise it for a stricter scale.

### Mixed text sizes: score the smallest, never the average

A screen mixes sizes: body text, a status bar, footnotes, small-caps labels. A whole-screen
recall is an average, the same trap as SSIM: a status bar holding 3% of the words can
vanish while the score barely moves. So every verdict is on the **smallest text size with
real content**:

- each OCR line's height is its tallest word box;
- the smallest size is the lowest height `h` whose lines within `[h, 1.2h]` hold at least
  24 letters or digits, so an icon misread as "e" or a lone badge digit cannot pin it;
- recall is measured on that group's words only.

Unit tests pin the two edges: one small status line among 40 body lines sets the floor;
two stray single-character specks do not. The falsifiable end-to-end check
(`scripts/check-legibility.py mixed`) renders 36 px body text with and without an 18 px
status bar holding ~3% of the words: the bar raises the text-safe scale from x0.250 to
x0.471 and keeps the bar at 8.2 px; without it the scale drops back.

### Results

`calibrate --from`, smallest-text scoring:

| input | smallest text (OCR line) | calibrated | DPI-derived |
|---|---|---|---|
| DISPLAY1, 200%, live screen | 12 px, 10 words | 0.435 | 0.5 |
| WezTerm terminal on DISPLAY5, 125% | 9 px, 90 words | 0.736 | 0.8 |
| Sublime Text on DISPLAY1, 200% | 14 px, 33 words | 0.389 | 0.5 |
| synthetic Consolas 16 / 24 / 32 / 48 px | 14 / 22 / 28 / 45 px | 0.736 / 0.435 / 0.343 / 0.250 (search floor) | n/a |

The synthetic sweep (`scripts/check-legibility.py sweep`) is the falsifiable one: the OCR
line height that survives the calibrated scale is **9.6-10.3 px across a 3x range of font
sizes** (48 px hits the search floor and is excluded). That is *Windows OCR's* threshold.
Claude's own is lower (§5d), so `calibrate` and `--scale auto` are conservative for Claude.

**Caps and digits read small.** A line box runs from the tallest letter to the lowest
descender, so caps-only or digit-only text (a clock, SMALL-CAPS LABELS) measures at cap
height, smaller than its real size: measured at 0.79-0.90x the rendered height for caps and
0.96-1.00x for digits and mixed case. Any rule applied to OCR heights therefore errs large
on caps: an extra tile, never lost legibility.

### How `--scale auto` decides

1. the per-display value cached by `winshot calibrate`
   (`%LOCALAPPDATA%\winshot\calibration.toml`, keyed by display name), else
2. `1 / DPI scale`.

For a capture spanning several displays it takes the **largest** (least aggressive) of
their scales. Calibration is not cached by `--from`, and nothing has been cached on this
machine yet: calibrate while a terminal or editor fills the display.

The "measured glyph height via edge-density autocorrelation" signal from the original
design was **not implemented**. OCR line heights measure glyph size directly.

### 5b. The default output: Claude's vision limits without losing text

This is winshot's **default** mode (the project is for LLMs; the older spelling
`--for-claude` is still accepted). `--raw` gives the lossless, full-resolution original.

Claude's vision guidance is ~1568 px on the long edge (2000-2500 for dense text) and a hard
rejection above 8000 px. Claude Code itself shrinks what it shows Claude: in this session a
3840x2400 capture arrived as 2000x1250 and the 5760x2711 desktop as 2000x941. A plain resize
to fit keeps the aspect ratio, so on a tall or wide image the short edge collapses with the
long one: an 1800x11846 retina page fit to 2000 px is x0.169, and its smallest text goes from
17 px to 2.9 px.

**The text decides the scale; the number of images absorbs the length.**

1. Measure: OCR the full-resolution image (in bands cut at blank rows when it exceeds the
   engine's 10000 px maximum) and find the smallest text size (§5, same rule).
2. Scale so that text lands at `SAFE_LINE_PX = 8`, the height measured on Claude itself
   (§5d): `s = 8 / h`. OCR is the ruler here, not the judge: it needs ~10 px, so letting it
   verify would push every scale back up to its own limit. With too little text to
   measure, fall back to calibrated or 1/DPI, and say so.
3. Fit or split: if fitting the long edge is within 5% of `s`, one image. (3840 wide at
   2000 px is x0.521, so a measured x0.53 still gives one image.) Otherwise keep `s` and
   split the long axis (wide images are transposed, so one code path serves both).
4. Cut: tiles are balanced (`ceil(length / long_edge)` of them), and each cut goes in the
   blank row nearest the balance point within a quarter tile. A row is blank when its
   horizontal detail averages under 2 per pixel, which tolerates a divider line crossing it
   and rejects any row through glyphs. With no blank row there, the cut overlaps the next
   tile by 1/16 of the long edge, so a sliced line appears whole in one of the two.
5. A multi-monitor desktop splits at monitor edges first. Tiles are named `-1of4` ... and
   printed in reading order. More than 8 tiles prints a token-cost note (exact counts, §5c).

Measured on this machine (`winshot -v`):

| input | smallest text | fit would be | output | smallest text after |
|---|---|---|---|---|
| retina page 1800x11846 | 17 px, 82 of 303 lines | x0.169 (2.9 px) | 3 tiles at x0.485, all seams blank | 8.2 px |
| DISPLAY1 3840x2400 | 13 px caps labels, 5 of 90 lines | x0.521 (6.8 px) | 2 tiles at x0.620, blank-column seam | 8.1 px |
| whole desktop 5760x2711 | per monitor | x0.347 | DISPLAY1 as above + DISPLAY5 unchanged (8 px digits) | 8 px |

Measuring costs 0.2-0.7 s (one OCR pass; longer on tall pages, which OCR in bands), and is
skipped with `--max-detail` when the image already fits. The DISPLAY1 row is the caps effect
above: its labels measure 13 px but render at ~16 px, so x0.521 would leave them at ~8.6 px,
inside Claude's range. The rule spends a second tile for margin; `--text-scale 0.52` accepts
the thinner margin as one image.
`--long-edge` sets the target (256-8000). Every output is <= 8000 px by construction, and a
normal capture larger than that warns that Claude would reject it.

A 2-D grid is not implemented: an image too big on *both* axes at the text scale shrinks
its short edge to fit and warns that text may fall below legible size.

### 5c. Exact token cost, and what saves tokens without losing text

**The cost is exact, not estimated.** Claude reads an image as 28x28-pixel patches, one
visual token each: `ceil(w/28) x ceil(h/28)` (docs: vision, "Resolution and token cost").
Two tiers cap every image, and anything over either cap is downscaled server-side:

| tier | models | max edge (after padding to 28) | max visual tokens |
|---|---|---|---|
| high (`--tier high`, default) | Claude 4.7 and later | 2576 px | 4784 |
| standard (`--tier standard`) | older, e.g. Haiku 4.5 | 1568 px | 1568 |

`tile::api_size` ports the docs' reference resize (binary search, round-half-even) and is
unit-tested against every example in the docs. One disagreement found: for 2000x1500 on the
standard tier the docs' table says 1269x952 and the docs' own code says 1270x952 (same 1564
tokens). Claude Code adds its own cap of 2000 px on the raw long edge (3840x2400 arrived here
as exactly 2000x1250). `-v` now prints, for every image written, the size Claude will see
and its exact token count; the old `w x h / 750` estimates are gone.

`scripts/count-tokens.py` checks all of this against the API itself: it asks the free
`count_tokens` endpoint for each image's cost and marks each image
`"oversized_image": "error"`, so the endpoint rejects anything the API would resize.
**Run 2026-09-23 on claude-opus-5-5, 9 images (304x2000 to 2000x1250): every image billed
exactly its patches + 3.** The +3 is a fixed per-image-block overhead, now in `tile::billed`,
and `-v` matches the API on all 9. The raw 3840x2400 capture was rejected as "would be
downsized to 2420x1512", the size `tile::api_size` predicts to the pixel.

**What does not save tokens.** File size and format: the cost depends only on pixel
dimensions, so PNG compression, palettes and WebP save bytes and zero tokens, and lossy
formats would lose text for nothing.

**What does, without losing text** (all in the default mode, measured on this machine):

1. **Never let the API resize.** Tiles now respect the visual-token cap as well as the edge:
   a 2000x2000 tile is 72x72 = 5184 tokens, over 4784, and would have been shrunk after
   winshot verified its text scale. A fidelity fix, not a saving.
2. **Patch alignment, free resolution.** A 1059 px side already pays for 38 patches =
   1064 px, so the scale grows to fill the last patch: more pixels, same tokens.
3. **Trim uniform margins.** Rows and columns matching the corner colour carry nothing;
   `tile::trim` crops them (keeping 6 px). Tall page: 1800x11846 -> 1732x11735.
4. **Lean by default.** An image that already fits is shrunk to its verified text-safe
   scale instead of sent at full size. `--max-detail` keeps full size for non-text detail.
   A 1920x1080 image whose smallest text is safe at x0.6 drops from 2,691 to 1,008 tokens (-63%).

Exact tokens, confirmed by `count_tokens` on every output image (8 of 8 matched):

| input | as-is (what Claude sees) | OCR's 10 px threshold | **Claude's 8 px threshold** |
|---|---|---|---|
| retina page 1800x11846 | 304x2000 = 795 tokens, text 2.9 px | 9,324 (4 tiles) | **6,129** (3 tiles), -34% |
| DISPLAY1 3840x2400 | 2000x1250 = 3,243 tokens, text 6.8 px | 7,062 (2 tiles) | **4,564** (2 tiles), -35% |
| DISPLAY5 1920x1080 | 2,694 tokens | 2,694 | 2,694 (8 px digits: cannot shrink) |

Tokens scale with the square of the scale, so the threshold is the lever that matters most.

### 5d. Claude's own legibility threshold (measured)

`scripts/claude-legibility.py`, run 2026-09-23 on claude-opus-5-5 (48 calls, effort low;
estimated $0.40 beforehand, actual usage not logged): lines of random words (nothing to
guess from context), in Consolas and Segoe UI, as lowercase, ALL CAPS and digits,
downscaled so the rendered line lands at 5-16 px, and Claude asked to transcribe them. Word
recall against the ground truth. (The script labelled its lowercase case "mixed" at the
time; a review caught it. It is relabelled here, and the script now also has a real
Mixed Case set, not yet run.)

| font / shape | 5 px | 6 px | 7 px | 8 px | 10 px | threshold (90% of full size) |
|---|---|---|---|---|---|---|
| Consolas lowercase | 57% | 97% | 95% | 100% | 100% | 6 px |
| Consolas CAPS | 42% | 92% | 100% | 100% | 100% | 6 px |
| Consolas digits | 97% | 98% | 100% | 100% | 100% | 5 px |
| Segoe UI lowercase | 5% | 62% | 92% | 93% | 98% | **7 px** (worst) |
| Segoe UI CAPS | 82% | refused | 100% | 98% | 100% | 7 px |
| Segoe UI digits | 98% | 100% | 100% | 100% | 100% | 5 px |

Full size read 100% in every case. One call returned a refusal (random letter strings can
trip a classifier); it counts as no data, not a pass. Caps are *not* harder for Claude.

`SAFE_LINE_PX = 8`: the worst case needs 7, and at 8 every font and shape held at least
93%. OCR measures a line at 0.79-1.00x its rendered height (§5), so applying this rendered-px
threshold to OCR heights only ever errs toward more resolution.

Not covered: light-on-dark and low-contrast colour text, ClearType subpixel rendering,
other fonts and languages, and effort levels other than low. Real text with context is
easier to read than random words, so the result should be conservative there.

### Resampling filter

| filter | verdict |
|---|---|
| nearest | destroys text; not offered |
| bilinear | mushy below 0.75 |
| **Lanczos3** | sharp, slight ringing; default |
| Mitchell | softer, less ringing; may suit < 0.5 (not measured) |
| box / area | exact for integer ratios; used automatically for 1/2, 1/4, ... |

**With `--raw`, resampling is off unless `--scale` or `--fit` asks for it** (both require
`--raw`). It is never free for text. The default (LLM) mode resamples, but only down to the
measured text-safe scale (§5b).

---

## 6. Flash and shutter

Both **off by default**, and both started only **after** every frame is in memory.
Encoding runs while they play; the process waits for both before exiting.

**Flash.** Per captured rect, a layered, borderless, topmost, click-through window
(`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST`)
filled white, alpha ramped 180 -> 0 over ~120 ms, never activated. Verified with a
second process capturing mid-flash: mean luma 34.3 before, **111.8 during**, and 34.3 in
every shot that fired a flash.

**Testing both.** `winshot fx` fires the flash and the shutter with nothing captured
(`--display`, `--no-flash`, `--no-sound`, `--wav FILE` to save the sound, `--duration MS` to
hold the fade long enough to photograph). It reports whether Windows' `PlaySoundW` said the
sound played, and a capture with `--shutter` now warns if it did not.
`scripts/check-fx.py` automates the four claims on the real desktop: the sound plays and
the .wav is well-formed (44.1 kHz mono 16-bit, 140 ms); a second capture mid-flash is
brighter (measured as a fraction of the headroom to white, since a white flash on an
already-bright screen can only add a little: 229.6 -> 241.1, 45% of the way to white); a
`--flash` capture matches a plain one (+0.0%); and a `--shutter` capture reports no failure.
All four pass on this machine.

**Shutter.** Originally synthesized, with no product's audio asset used or reproduced:
two band-passed noise transients (3.8 kHz then a quieter 2.6 kHz, 75 ms apart, ~140 ms
total), fixed-seed noise so it is deterministic, rendered in memory by `fx::shutter_wav`
and played with `PlaySoundW(SND_MEMORY)`. The code is the generator, so there is no
`assets/shutter.wav` to keep in sync. A unit test pins the envelope and checks that the
spectral centroid falls within 2-6 kHz.

---

## 7. Crates

| need | crate |
|---|---|
| Win32 / WinRT (WGC, D3D11, DXGI, DWM, OCR, audio) | `windows` 0.62 |
| manifest | `embed-manifest` (build) |
| CLI | `clap` (derive) |
| PNG | `png` 0.18 |
| WebP lossless | `image-webp` |
| resize | `fast_image_resize` 6 |
| calibration cache | `serde` + `toml` |

OCR uses `Windows.Media.Ocr`, part of Windows, so `calibrate` needs no feature flag or
extra dependency. `rayon` was dropped: nothing needed it.

---

## 8. Open questions

1. **HDR**: does Windows' BGRA8 conversion look right on an HDR display? Needs an HDR panel.
2. **Concurrent per-monitor capture**: an estimated 50-100 ms on two displays; unmeasured.
3. **Cursor over WezTerm** on the 125% display was missing once; unexplained.
4. **`PrintWindow` fallback**: add it only if a real WGC failure is observed.
5. **Calibration on more content**: `RETAIN` rests on a synthetic sweep and a handful of
   real captures; more content types (browser UI, small fonts) would firm it up.
6. **Caps-only small text** makes the default mode err large (extra tiles). Normalising line
   height by letter shape (cap height vs ascender/descender) would tighten it.
7. **Claude's threshold on dark themes and colour text**: §5d measured dark-on-light
   greyscale only. Rerun `scripts/claude-legibility.py` with inverted colours to cover them.

