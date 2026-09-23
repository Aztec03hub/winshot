# Handoff

For the next session on this repo. Read [`README.md`](README.md) and
[`docs/DESIGN.md`](docs/DESIGN.md) in full first. DESIGN holds every measurement; this file
holds the state and the context you cannot infer from the code.

## What this is

winshot captures Phil's (`Aztec03hub`) Windows desktop from WSL **for LLMs to read**, not
for people: the default output is sized so Claude can read every word, and `--raw` is the
lossless, human-facing original (Phil's call, 2026-09-23). `winshot.exe` (Rust, WGC) plus a
bash shim replace the v0 in `legacy/`. **It is built, installed on
the target machine, and working**: `~/.local/bin/winshot` is the shim, the exe is at
`%LOCALAPPDATA%\winshot\winshot.exe`, and the old v0 shim is at `~/.local/bin/winshot.v0.bak`.

## Context you cannot inherit

**The v0 bug, and why it cannot come back.** v0 was DPI-unaware, so Windows silently
handed it logical pixels: 2.08x fewer pixels than physical on this rig, with no error.
winshot declares PMv2 in the manifest and then asserts it at startup, cross-checks every
monitor rect against its display mode, and refuses any monitor frame that is not exactly
physical size. The worst case is now a loud error. `legacy/winshot.ps1` is also fixed
(a `SetProcessDpiAwarenessContext(-4)` call before Windows.Forms loads).

**The earlier handoff's hardware table was wrong. Do not reuse it.** It listed DISPLAY5
as 3072x1728 at 200% and the virtual desktop as 6912x4338. Those were logical x2, not
measurements. Measured with both winshot and an independent DPI-aware PowerShell query:

| display | physical | logical | scaling | position (physical) |
|---|---|---|---|---|
| DISPLAY1 (primary) | 3840x2400 | 1920x1200 | 200% | 0,0 |
| DISPLAY5 | 1920x1080 | 1536x864 | 125% | -1920,1631 |
| virtual desktop | 5760x2711 | 3456x2169 | | -1920,0 |

So this rig **is** mixed-DPI, the hard case, and it is handled. The regression oracle is
"winshot gives 2.083x v0's pixel count here", **not 4x**. Displays get re-plugged; run
`winshot list displays` rather than trusting any table, including this one.

**Several original design claims were measured and refuted**; DESIGN now says so in place:
- "<256 colours is common": 0 of 8 real captures qualify. The palette path stays (it costs
  ~1 ms) but the size win comes from zlib "balanced".
- "WebP lossless is 25-35% smaller": it was larger on 5 of 8 captures.
- "Encode dominates the budget": WGC capture dominates (~150-250 ms); encode is ~100 ms at 15.6 Mpx.
- "~5x faster than v0": 1.5x faster than a *correct* v0 (413-443 ms vs 641-671 ms).
- "OCR agreement >= 99.5%": unreachable with Windows OCR; calibration uses word recall
  relative to OCR's own noise floor, scored on the **smallest text size only** (DESIGN §5).
  Windows OCR needs ~10 px; **Claude itself reads random words down to 6-7 px** (measured,
  DESIGN §5d), so the default mode uses `SAFE_LINE_PX = 8` with OCR as the ruler, not the judge.

## State of each original build step

| step | state |
|---|---|
| DPI-aware `list displays` | done, triple-checked (see above) |
| full-desktop capture | done, mixed-DPI composite verified |
| `list windows`, `--window` / `--pid` / `--exe` | done; also `--window 0xHWND`, blank helper windows skipped, minimized restored |
| `--display`, `--region`, `--all` | done; negative coordinates work |
| encoder benchmark | done (`--bench-encode`, hidden); `png` balanced chosen |
| palette path | done, round-trip unit-tested; prior measured and refuted |
| resize | done; Lanczos3, area averaging automatically for exact 1/2, 1/4 |
| `calibrate` | done, via built-in Windows OCR; **no feature flag needed** |
| flash + shutter | done, fired after capture; `winshot fx` previews both, `scripts/check-fx.py` verifies them |
| WSL shim + install | done (`shim/winshot`, `install.sh`) |
| LLM-sized output, **now the default** (`--raw` for lossless) | done: smallest text sets the scale, balanced tiles cut between lines, per-monitor split, `--from` for saved PNGs |
| downscale-not-smaller warning | done: a `--scale`/`--fit` result no smaller than full resolution warns |
| exact token counts | done: `ceil(w/28) x ceil(h/28)` and the API's resize rule, unit-tested against the docs; `-v` prints them |
| token savings | done: token-cap-aware tiles, margin trim, patch alignment, lean default (`--max-detail` opts out) |
| API confirmation | **done 2026-09-23**: `count_tokens` matched winshot on every image (patches + 3); Claude legibility measured (estimated $0.40; usage not logged) |

A stage-by-stage walkthrough of the default mode with real images (tall page, laptop screen,
desktop) is published at https://claude.ai/artifact/Qu2XRCf5ScDqospmara6PZ (private to Phil).

## Not done, deliberately

- `PrintWindow` fallback: no WGC failure observed; the black-frame warning would expose one.
- Edge-density glyph-height signal: superseded by OCR calibration.
- Palette sorted by frequency: first-seen order; no measured effect.
- No calibration is cached on this machine yet. Run `winshot calibrate --display 1` with a
  terminal filling the display; `--scale auto` uses DPI-derived values until then.

## Open (DESIGN §8)

HDR is untested (no HDR panel here). Capturing the monitors concurrently is the next speed
lever. One `--cursor` capture over WezTerm showed no cursor, unexplained. The default mode errs
large on caps-only small text (OCR measures caps at 0.79-0.90x height: extra tiles), and
Claude's threshold was measured on dark-on-light text only.

The API key is `ANTHROPIC_API_KEY` in `~/.bashrc`, deliberately **not exported** (an exported
key can switch Claude Code itself to API billing). Pass it per process:
`ANTHROPIC_API_KEY=$(bash -ic 'printf %s "$ANTHROPIC_API_KEY"') python3 scripts/...`. The
older `ZNTHROPIC_API_KEY` line is a revoked key (401).

## Verify

```bash
./build.sh test                        # 18 unit tests
python3 scripts/check-legibility.py    # OCR sweep, mixed sizes, tall page
python3 scripts/check-fx.py            # flash on screen and not in the shot; sound plays (you will see and hear it)
winshot -v                             # must print physical == display mode for every display
```

## Phil's working preferences

- Wants it fast and genuinely useful, not a demo. Do not oversell the language: the win is
  architectural, and now also measured as more modest than first claimed.
- Prefers measured claims over confident ones. Label estimates as estimates.
- No em-dashes in anything written for this repo; hyphens are fine.
- Read every project markdown file in full before working.
- Repo is public under `Aztec03hub`.
