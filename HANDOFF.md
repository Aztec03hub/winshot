# Implementation handoff

For the session that builds this. Read [`docs/DESIGN.md`](docs/DESIGN.md) in full first —
it holds the reasoning, this holds the context you cannot infer from the repo.

## What this is

Phil (`Aztec03hub`) needs screenshots of his Windows desktop from WSL. A working v0 exists
in `legacy/` (bash + PowerShell). This repo replaces it with a Rust Windows binary plus a
thin WSL shim.

## Context you cannot inherit

**The v0 has a real bug, and it is the whole reason for the rewrite.** PowerShell's
`System.Windows.Forms` is DPI-unaware, so it captures the *logical* 3456x2169 instead of
the *physical* 6912x4338 — half the linear resolution, 4x the pixels lost, with no error
or warning. It was found by comparing a DPI-unaware enumeration against a DPI-aware one,
not by looking at the images, which appear fine. **Verify DPI awareness early and assert
on it**; it is silent when wrong.

**The target machine** (do not assume it generalises):

| display | physical | logical | scaling |
|---|---|---|---|
| DISPLAY1 (primary, Intel, 4K laptop) | 3840x2400 | 1920x1200 | 200% |
| DISPLAY5 (NVIDIA RTX 5090 Laptop) | 3072x1728 | 1536x864 | 200% |
| virtual desktop | 6912x4338 | 3456x2169 | |

Both at 200%, so `--scale auto` = 0.5 here. **Do not hardcode that** — a uniform-DPI rig
is the easy case and mixed DPI is the one that breaks naive code.

**Empirical data point worth keeping:** a 0.5-scale capture of a dense terminal (small
monospace log output) was read back without difficulty. That validates the
DPI-derived signal at 200%, and is the anchor the OCR calibration should reproduce. It is
one observation on one rig, not a general result.

**Known-unverified claims in the design doc**, flagged honestly:
- All Rust-side timings in §2 are *estimates* except the PowerShell startup cost. Measure
  before quoting them anywhere.
- The "<256 colours" claim for UI screenshots is a reasonable prior, **not measured on
  Phil's actual screenshots**. Measure it on real captures before building the default
  path around it — if it is rare in practice, the palette optimisation is not the free
  lunch the design assumes and the default size story needs rethinking.
- WGC's `IsBorderRequired` behaviour on this specific Windows build is unverified.

## Build order

1. **Skeleton + `list displays`** — DPI-aware enumeration. Assert physical == 6912x4338
   on this machine; that single check catches the v0 bug class immediately.
2. **Full-desktop capture -> PNG.** Establish correctness before any optimisation.
3. **`list windows` + per-window / per-pid / per-exe capture** via WGC.
4. **Per-display and `--region`.**
5. **Encoder benchmark** (`fpnge` vs `zune-png` vs `image`) — encode dominates the budget,
   so pick on data, not reputation.
6. **Palette path** — measure the colour-count prior first (see above).
7. **Resize** (`fast_image_resize`, Lanczos3 + area for exact 1/2).
8. **`calibrate`** behind a feature flag so OCR stays out of the default build.
9. **Flash + shutter**, both off by default, both fired *after* capture.
10. **WSL shim + install script.**

## Non-negotiables

- `PER_MONITOR_AWARE_V2` via **manifest**, not a runtime call.
- Flash and shutter fire **after** the frame is grabbed, or the flash is in the shot.
- Shutter sound must be **originally synthesized** — never bundle or reproduce Apple's
  or any other product's audio asset. Generate it from a script in-repo.
- JPEG must never become the default format. DCT ringing wrecks text.
- Never silently save a black frame: DRM-protected content returns black from WGC, so
  detect and warn.
- Report the actual captured resolution in `--verbose`, so a DPI regression is visible.

## Testing

- Golden test: capture a known window, assert dimensions match the DPI-aware query.
- Palette round-trip: indexed PNG must decode **bit-identical** to the truecolor source.
- Downscale sanity: `calibrate` on a synthetic image with known glyph sizes should return
  a scale that keeps x-height above the threshold.
- The v0 comparison is a useful regression oracle: the new binary must produce **4x the
  pixel count** of `legacy/winshot.sh` on this machine.

## Phil's working preferences

- Wants it fast and genuinely useful, not a demo. He asked "how do we make it SUPER
  powerful *and* fast" — but the honest answer, already in the design, is that the win is
  architectural (no PowerShell, WGC, fast encoder) rather than from choosing Rust. Do not
  oversell the language.
- Prefers measured claims over confident ones. If something is estimated, say so.
- Repo is public under `Aztec03hub`.
