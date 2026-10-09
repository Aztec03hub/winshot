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

**OCR boxes depend on the letters.** A line box runs from the highest to the lowest ink, so
caps-only or digit-only text (a clock, SMALL-CAPS LABELS) is boxed at cap height: 0.79-0.90x
its rendered height for caps, 0.96-1.00x for digits. `calibrate` uses raw OCR heights and so
errs large on such text; the default mode converts every box to a real height first (§5d).

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
2. Group the lines by **letter shape** (lowercase text / CAPS / digits), estimate each
   group's smallest real line height from its OCR boxes (§5d: a box only spans the
   letters it contains), and scale so every group stays at or above the height Claude was
   measured to read for that shape: **text 9 px, caps 7 px, digits 6 px**. The most
   demanding group sets the scale. OCR is the ruler here, not the judge: it needs ~10 px,
   so letting it verify would push every scale back up to its own limit. With too little
   text to measure, fall back to calibrated or 1/DPI, and say so.
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

| input | binding text | fit would be | output |
|---|---|---|---|
| retina page 1800x17886 | 17 px lowercase code (brackets: full-height box) | x0.112 (1.9 px) | 5 tiles at x0.533, all seams blank |
| DISPLAY1 3840x2400 (diff page) | 15 px CAPS labels | x0.521 | **1 image at x0.421** (1617x1008) |
| whole desktop 5760x2711 | per monitor | x0.347 | one file per monitor, each by its own text |

Measuring costs 0.2-1.2 s (one OCR pass; longer on tall pages, which OCR in bands), and is
skipped with `--max-detail` when the image already fits. The rule cannot tell real UI text
from text inside a picture: a screen showing screenshot thumbnails protects their tiny text
too (seen live: 9 px labels inside an embedded thumbnail forced 2 tiles).
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

How the laptop screen (DISPLAY1, the diff page) moved as the rule was refined; exact
tokens, confirmed with `count_tokens`, and read fidelity from `scripts/fidelity-check.py`
(Claude transcribing each version against its own full-resolution reading, 241 words):

| rule | output | tokens | words read back |
|---|---|---|---|
| OCR's 10 px, one threshold | 2 tiles | 7,062 | not measured |
| Claude's 8 px, one threshold | 2 tiles | 4,564 | 97.9% |
| per-shape thresholds | 1 image, x0.521 | 3,243 | 97.5% |
| **+ letter-class box ratios** | **1 image, x0.421** | **2,091** | **97.5%** |

-70% tokens with no measurable read loss: every version misses the same half-scrolled-off
code line, and the 0.4-point gap is a single "3". (An earlier run reported 94.0% vs 98.4%;
that was a scoring artifact, the reference's "Image 1:"..."Image 4:" labels counted as
words. `fidelity-check.py` strips them now.) Tokens scale with the square of the scale, so
the threshold and the height estimate are the levers that matter most.

### 5d. Claude's own legibility threshold (measured)

`scripts/claude-legibility.py` on claude-opus-5-5, effort low, 2026-09-23: lines of random
words (nothing to guess from context), Consolas and Segoe UI, downscaled so the rendered line
lands at 5-16 px, and Claude asked to transcribe them; word recall against the ground truth.
Three runs: light theme (48 calls), dark theme (64), and a fill run (22) that retried
refused cells with new words and added 9 px and light Mixed Case. Worst font and theme per
cell ("r" = refused: random letter strings can trip a classifier; no data, not a pass):

| shape | 5 px | 6 px | 7 px | 8 px | 9 px | 10 px |
|---|---|---|---|---|---|---|
| lowercase (Segoe, dark) | 5% | 58% | 87% | 90-93% | 97% | 100% |
| Mixed Case (Segoe, dark) | r | r | 78-87% | r (x2) | 98% | 95% |
| Mixed Case (Segoe, light) | - | - | r | r | 95% | - |
| lowercase / Mixed (Consolas, dark) | r | 77-87% | 90-93% | 95-97% | 98% | 100% |
| CAPS (worst of all four) | 27% | 83% | 98% | 98% | - | 100% |
| digits (worst of all four) | 97% | 98% | 100% | 100% | - | 100% |

Full size read 100% in every case. **Thresholds used** (`calibrate::Shape::safe_px`):
text **9 px** (Segoe lowercase and Mixed Case on dark are borderline at 8 and 95-98% at 9),
caps **7 px** (98-100% at 7 in every font and theme), digits **6 px**. Dark themes are
harder than light for lowercase; caps and digits are not.

**From OCR boxes to real line height.** Windows OCR boxes a word from its highest to its
lowest ink, so the box depends on the letters in it. Measured on Consolas and Segoe UI,
light and dark, box / (ascender top to descender bottom):

| letters in the line | box ratio (all four agree within 0.04) |
|---|---|
| tall and dropping (`height`) | 1.000 |
| tall only, no g/j/p/q/y (`added`) | 0.769-0.778 |
| dropping only | 0.744-0.778 |
| x-height only (`mono`) | 0.513-0.556 |
| CAPS / digits | 0.718-0.722 |

Every line's real height is its box divided by the largest ratio for its own letters, before
lines are grouped, so the estimate errs small and the scale large. Brackets, commas, slashes,
`j`, accented letters and unknown marks can stretch a box, so they make a lowercase line
full height and a caps or digit line 1.45 (a full-height box over capitals). Quote marks and
`t`, `i` reach above x-height and count as tall. A review of the first version found four
ways it could err the other way (a group ratio taken from the wrong lines, quotes treated as
x-height, `j` treated as tall-only, caps with brackets); all four are fixed and unit-tested. Without this, "added removed" (17 px box, 22 px
real) read 22% too small and forced the laptop to x0.529 instead of x0.421. The smallest
group also needs real content at its own size (12+ characters within 10%), so a lone 3-letter
tab title just under a group of labels cannot pin the whole group to its size.

Not covered: low-contrast colour text, ClearType subpixel rendering, other fonts and
languages, and effort levels other than low. Real text with context is easier to read than
random words, so the thresholds should be conservative there.

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

### 6b. The hotkey

`winshot hotkey` (`src/hotkey.rs`) is a global shortcut: one key combination, from any
program, screenshots the focused display, saves it under a fresh name and puts it on the
clipboard. Everything below was built and run on this machine on 2026-10-09.

**Mechanism.** One hidden message-only window and a message loop, with the combination
registered by `RegisterHotKey`. That is system-wide and needs no administrator rights, and in
normal use installs no keyboard hook (a low-level hook is what antivirus products flag, and it
sees every keystroke). The one exception is the short-lived hook while a remote-desktop client
has focus, described below. The same window is the single-instance lock and the target of `--stop`
(`WM_CLOSE`), so there is no pid file to go stale. A bare key with no modifier is refused,
since it would be taken from every program. A one-off probe (not kept in the repo) registered
seven candidate combinations (ctrl+`, ctrl+., each with alt and with shift, ctrl+alt+shift+s):
all were free, so no running program owned any of them globally. That probe cannot see
shortcuts inside one app, which still lose to a global hotkey: ctrl+` is also Sublime Text's
console and VS Code's terminal.

**Which display.** The monitor holding the foreground window (`GetForegroundWindow`,
`MonitorFromWindow`); when nothing is focused or the window is minimized, the monitor under
the mouse. Checked by hand with injected key presses, Sublime focused on DISPLAY5 and Chrome
focused on DISPLAY1: each press saved that display (not covered by `check-hotkey.py`).

**Bursts.** A press is split in two so that quick presses each get their own frame. The daemon
thread does only the fast half the instant the key goes down (find the focused display, grab its
frame, flash, shutter); a worker thread does the slow half in order (measuring the text, tiling,
encoding, files, clipboard, pruning). Before the split everything ran on the daemon thread, about
0.35-0.4 s a press by the log, so a burst queued up and each frame was grabbed when its turn came,
not when the key was pressed; one burst in the log gave 11 files 0.39 s apart. The file name
carries the time of the grab, not of the save.

Three rules came out of review. **The daemon thread never blocks on the worker.** It must keep
pumping messages (`--stop`, the remote-desktop focus watcher), so when 7 screenshots are already
unsaved (6 waiting plus the one being saved, `QUEUE_CAP`) a further press is dropped: no frame is
taken, a line goes to the log and the error beep sounds. A frame is a whole screen (about 27 MB at
4K, so 7 of them are about 190 MB, plus the worker's working copies of the one it is saving), which is why
the backlog is bounded;
frames that were dropped are lost, not late. **The daemon window owns the clipboard, and its thread
always pumps.** Clearing the clipboard sends a synchronous message to the previous owner, so the
owner's thread must keep handling messages. A version that gave the worker a window of its own
(never pumping) was tried and dropped on review: another program clearing the clipboard later could
in theory have waited on it. A measurement here (a throwaway script, not kept in the repo: a separate
process taking the clipboard after a press, 434 ms, mostly its own start-up, once) showed no hang, but one measurement of Windows internals is
not a reason to keep a design that needs it. **Stopping lets frames already taken finish saving**:
`WM_CLOSE` stops the loop but leaves the window alone, the queue is closed, the daemon thread keeps
pumping while it waits for the worker, and only then is the window destroyed. `running()` finds the
daemon by that window, so `--stop` really does wait for the saving (up to its 5 s limit, which a very
long backlog could exceed; it then says so rather than claiming success). The daemon itself gives up
after 20 s and logs how many screenshots it abandoned, because clearing the clipboard waits on the
window of the program that owned it, and a hung one would otherwise keep the process alive for good.
The remote-desktop keyboard hook is removed before that wait, so it cannot swallow the key while the
backlog drains.

Measured with `check-hotkey.py` on this machine, with posted presses and no keys: 7 presses 40 ms
apart gave 7 files whose grab times lay 75-109 ms apart (523 ms in all; an earlier run of 8 gave
52-93 ms), and the clipboard held the last; 14 presses gave 9 files, the rest dropped, the hotkey
answering afterwards. Those are single runs. What limits the gap between grabs was not measured
(the guess is the capture API's wait for a first frame); a held key is still one press (its
repeats are swallowed on purpose). The assertion that guards this (gaps under 250 ms, span under
150 ms a press) would fail on the old inline code, which gave gaps of 350 ms and more.

**Pipeline.** The same sizing path as a plain `winshot` run: `capture_display` (with its exact
physical-size check) then `for_claude` (measure the smallest text, scale, tile). The flash
and shutter start after the frame is in memory, as always. Differences from a plain run: the
cursor is never included, and the daemon's stderr is the null device, so what `shoot` would
print goes to the log instead (the all-black-frame warning is logged; the "too little text to
measure, using the fallback scale" warning is not, and is not visible anywhere).

**When a press fails.** It logs, beeps and the daemon carries on. A panic is caught per press
(and logged by a panic hook, since stderr is lost); a failed grab is retried once with a new D3D
device (it can die across sleep or a display change); the folder is recreated if it vanished;
the clipboard is filled with every payload built before it is cleared, and a failed put empties
it again rather than leaving it half-set (a panic mid-fill is caught and the clipboard closed,
but could leave it partly filled). A failure at start-up (a taken key) is
reported by the launching command from the log's last line.

**Names.** `<yyyy-mm-dd>_<hh>_<mm>_<ss>_<ms>-<window title>.png`, built by appending, never
with `Path::with_extension`, which would cut a title like "report.docx - Word" at its last
dot (found by reading the code, now unit-tested). The title is stripped of characters
Windows forbids and cut to 60. A name already in the folder, as one image or as tiles, gets
`~2`, `~3`. Three presses 0.4 s apart gave three distinct files.

**The clipboard.** One clipboard holds several formats and each program asks for the one it
understands, so winshot sets all of them in one transaction: a DIB (24-bit, bottom-up) and
the registered `PNG` format for programs that paste images, the file's Windows path as
Unicode text for terminals and editors, and a `CF_HDROP` file list for Explorer, Teams and
Outlook. Windows adds the derived bitmap formats itself. Read back after a real press: the
text and the file list equal the saved file's path, and the bitmap has the saved file's
size. The text is a **Windows** path because that is what a Greenshot capture pastes as
(into Sublime it was `C:\...\Screenshots\<date>-<title>.png`) and Claude Code in WSL
accepted that: it showed the attachment as `[Image #n]` with source `/mnt/c/...`,
converting the path itself. A multi-tile result carries paths only, since a clipboard holds one image.

**Detaching.** `winshot hotkey` re-launches itself with `DETACHED_PROCESS` (no console of its
own) and returns once the new process has its window and is still alive 150 ms later, or reports
its exit with the last log line that run wrote.
First version hung the WSL shim for as long as the daemon lived: `CreateProcess` gives a
child **every** inheritable handle, not only the three standard ones, so the shim's output
pipe never reached end-of-file. Giving the child null stdio was not enough; the parent now
marks its own standard handles non-inheritable before spawning. After the fix the shim
returns in 0.28 s (3 launches; it includes the 150 ms re-check that the child is still alive).

**Autostart** is a value under `HKCU\...\Run` (written with `reg.exe`) holding the command
line (each argument quoted for `CommandLineToArgvW`, a trailing backslash doubled), so the key,
folder and prune options persist; the key and folder are validated before anything is written,
and `--dir` is stored absolute (the shim translates WSL paths for it). `--stop` waits until the
window is gone, because a restart or an install right after must not race the exiting process.
`install.sh` stops a running hotkey before copying the exe (it is locked while it runs) and says
to start it again. **Pruning** (`--prune-days`, at least 1) only deletes files whose names carry
winshot's full stamp (`2026-10-09_10_48_16_417`, then `-title`, `~n` or nothing, then `.png`),
after each press and at start; a `2024-trip.png` in the same folder is never touched, and
nothing is deleted unless asked.

**Tests.** 32 unit tests in all (9 in the hotkey module) cover everything pure there (key parsing, title sanitising, name building
and same-millisecond collisions, the stamp test and decoys for pruning, argument quoting, the DIB
and file-list layouts). `scripts/check-hotkey.py` runs the rest on the real desktop: start,
a second start, one press (file name; clipboard text and file list equal the file's path; the
clipboard bitmap equals the file on a 16x16 grid of pixels and its PNG payload equals the file's
bytes; or, for tiles, every path in order and no image), three quick presses with no name reused,
`--stop` and restart. By default a "press" is `WM_HOTKEY` posted to the daemon's window, so
nothing is typed into whatever has focus; `--inject` sends the real chord. That the
`RegisterHotKey` binding itself fires was shown once with injected keys (Ctrl+` and ctrl+F24).

**Remote desktop (measured 2026-10-09).** Capturing a live remote-desktop session always worked:
the session is pixels on a local display. The key did not. With `mstsc` showing a live session
(`phil-desktop - Remote Desktop Connection`) in the foreground, neither an injected ctrl+F24
nor, later, Phil's real ctrl+` and win+shift+` reached the daemon (no press, no error); with any
other local window focused they did.

*Cause.* Web research (AutoHotkey forum threads on the same symptom; Microsoft's
`KeyboardHookMode` documentation) says the remote-desktop client installs its own low-level
keyboard hook while its window is active. A low-level hook runs before `RegisterHotKey` is
applied, and the most recently installed hook runs first, so the client sees the key first and
forwards it to the remote machine. The "Apply Windows key combinations" setting only decides
where Windows-key shortcuts go and did not rescue win+shift+` here. That the client's hook is
what swallowed the key is the documented explanation, consistent with every observation, but
the client's own hook was not inspected.

*Fix (`rdp` module in `src/hotkey.rs`).* A `SetWinEventHook(EVENT_SYSTEM_FOREGROUND)` watcher
sees which program has focus. When it is a remote-desktop client (`mstsc.exe`, `msrdc.exe`,
`msrdcw.exe`, `windows365.exe`, `wfica32.exe`, `cdviewer.exe`, `vmware-view.exe`), a thread of
its own installs a `WH_KEYBOARD_LL` hook, 400 ms after focus and again at 1.5 s: installed after
the client's, it runs first. The hook matches the key and the exact set of held modifiers, posts
the same `WM_HOTKEY` message to the daemon window, and swallows the key, its auto-repeat and its
key-up so the remote machine does not receive it. Leaving the client stops the thread and removes
the hook, so the hook exists only while a remote session has the keyboard. `--no-rdp-hook` turns
it off. It logs how many key events it saw (never which keys), to tell "installed but the client
was first" (0) from "working".

Two design points came out of review. **The hook has a thread to itself**, which does nothing but
pump messages: a low-level hook only runs while its thread is inside `GetMessageW`, and the
daemon thread is busy for about a second per screenshot, so on that thread every key typed during
a press would have waited out the system's hook timeout (300 ms by default; Windows can also
drop a hook that times out), and the key-up of the press itself could have been missed, leaving
the key "held" so that the next press was swallowed (the held state now also expires: a key-down
more than 1.5 s after the last swallowed one is a new press, not a repeat; the longest keyboard
repeat delay Windows offers is 1 s, and a real second press has a key-up before it, which clears
the held state). The decision is a pure function (`rdp::decide`) with unit tests. The hook thread
is started once per client window, not on every focus event, so a child window or the connection
bar taking focus and handing it back does not tear down a working hook, and a replaced hook keeps
the held state. **The registered
hotkey is never removed**: an earlier version unregistered it while the hook was in charge, which
left the key dead for the first 400 ms after focus, and dead for good if the hook failed to
install or the hotkey could not be registered again. With both in place, the hook takes the key
and swallows it when it is first in line, and the registered hotkey is the fallback when it is
not.

*Result.* With a real keyboard, in the same live `mstsc` session (first version, before the
review changes): one press of ctrl+` saved `...12_03_49_761-phil-desktop - Remote Desktop
Connection.png`. The log shows mstsc.exe gaining focus at 12:03:47, the first hook's removal at
12:03:48 with 0 events seen, the capture at 12:03:49, and the hook reporting 7 events when focus
left at 12:03:53.

The version with the hook on its own thread and the hotkey kept registered was tried live
afterwards: three ctrl+` presses a few seconds apart in the same session gave three files
(`...13_13_25_645`, `...13_13_28_913`, `...13_13_32_254`), the log showing the hook installed once
for the window and seeing 3, 98 and 15 key events over the stretches that followed. The three
presses were seconds apart, so it did not exercise a press during a press, a held key, or a
second press inside the second after the first. A review then changed the hook's lifetime
(started once per window, held state kept across a reinstall, a longer repeat window); **that
last version passes its unit tests and the end-to-end check but has not been tried live.**

*Limits.* Not tested: why two install times (the log shows the first hook removed unused and the
press arriving after the second install, which is consistent with the first being too early, but
nothing proves it); other clients (the Windows App's executable name is a guess); a client in
full-screen mode; a client that re-installs its own hook later than we do. **An elevated client
is not covered by the hook**: Windows does not show a normal program the keys typed into an
elevated window, so the hook sees nothing there and only the registered hotkey applies. **Alt-
and Win-based combinations leak a lone modifier** to the remote machine (only the main key is
swallowed); with Ctrl and Shift, as in the default, that is harmless, but a lone Alt can open a
menu bar and a lone Win the Start menu.

**Not verified.**
- Claude Code's own image-paste key reading the bitmap off the clipboard from WSL (the path
  text is the tested route).
- Whether launching from the Run key flashes a console window at logon (never observed: the
  autostart was only switched on and off).
- HDR, and press-to-clipboard latency (never timed; a press measures the text, so it costs the
  0.2-1.2 s of section 5b on top of the capture, on the thread that also waits for `--stop`).

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
| hotkey, clipboard, message loop | `windows` (`RegisterHotKey`, `System::DataExchange`, `System::Memory`) |

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
6. **Sharp insets** (not built): the main image at body-text scale plus tiny crops of the
   few thin strips holding the smallest text (tab bar, legend, clock). Estimated 15-25%
   fewer tokens on screens like the laptop's; unmeasured.
7. **Hotkey in other remote-desktop clients** (section 6b): works in a live `mstsc` window
   through a short-lived keyboard hook; the Windows App / Windows 365, Citrix and VMware clients
   are on the list by executable name and untested, as is full-screen mode.
8. **Ruled out by Phil (2026-09-23):** altering colours (pre-sharpening, contrast boosts),
   sending an OCR transcript alongside the image (OCR errors would reach Claude as fact),
   and collapsing blank space (distorts layout and positions). Token savings must keep
   pixels' colours, the layout, and Claude reading the image itself.

