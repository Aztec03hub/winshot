# winshot

Screenshots of a **Windows** desktop **for LLMs**, driven from **WSL**.

Capture one monitor / one window / one process / an arbitrary region at true physical
resolution, and get back images sized so Claude can read every word on them: the smallest
text is measured and kept legible, long images are split into tiles, and nothing
downstream resizes them. `--raw` gives a lossless, full-resolution original instead.

> **Status: working.** Rust binary (`src/`) + WSL shim (`shim/`), built and verified on
> the target machine. Measurements behind every claim are in [`docs/DESIGN.md`](docs/DESIGN.md);
> what is still open is in [`HANDOFF.md`](HANDOFF.md). `legacy/` holds the v0, now DPI-fixed.

---

## For LLM agents

```bash
winshot                      # every monitor; prints one output path per line
winshot --window "CapCut"    # one window (title substring, or an hwnd from `winshot list windows`)
winshot --display 1          # one monitor
```

Read **every** path it prints, in order: a tall or wide capture comes back as tiles named
`-1of3`, `-2of3`, ... in reading order, and a whole-desktop capture as one file per monitor
(`-DISPLAY1`, `-DISPLAY5`). Each file is already inside Claude's image limits, so the text
size winshot checked is the size you see. `-v` adds, per file, the exact token cost.

`winshot --raw` is for a person: one lossless PNG exactly as on screen, which Claude Code
would itself shrink to 2000 px (and a tall one to unreadable).

---

## Why this exists

WSL has no access to the Windows desktop compositor, so a Linux binary **cannot** capture
the Windows screen, in any language. Something Windows-native has to do the capture. The
v0 shelled out to PowerShell; that worked but was silently wrong.

### The bug that motivated the rewrite

PowerShell's `System.Windows.Forms` is **DPI-unaware** unless told otherwise, and Windows
then hands it logical pixels with no error. On this machine (mixed DPI):

| display | physical | logical (what v0 got) | scaling |
|---|---|---|---|
| DISPLAY1 (primary) | 3840x2400 | 1920x1200 | 200% |
| DISPLAY5 | 1920x1080 | 1536x864 | 125% |
| **virtual desktop** | **5760x2711** | **3456x2169** | |

v0 captured the logical 3456x2169: **4x fewer pixels on DISPLAY1, 1.56x fewer on DISPLAY5**,
2.08x overall, and never said so.

winshot fixes it at three levels:

1. `PER_MONITOR_AWARE_V2` is declared in the **embedded manifest** (`build.rs`), not a runtime call.
2. At startup it **asserts** the process really is PMv2, and that every monitor's rect equals
   its display *mode* (which is physical by definition). Any mismatch aborts with an error.
3. Every monitor frame must be exactly the monitor's physical size, or nothing is saved.

A silent downscale can no longer happen; the worst case is a loud refusal. `--verbose`
prints physical and logical resolution for every display involved, so a regression is visible.

---

## CLI

```
winshot list displays                 # index, name, physical+logical res, scale, position, HDR
winshot list windows [--filter STR]   # hwnd, pid, exe, monitor, state, title

winshot                               # every monitor, one file per monitor (tiled if needed)
winshot --display 1                   # one monitor: index, name (DISPLAY5) or "primary"
winshot --window "CapCut"             # by title substring, or an hwnd from `list windows` (0x1411d6)
winshot --pid 27768                   # top window of a process
winshot --exe capcut                  # top window of an executable (.exe optional)
winshot --region -400,2000,1200,500   # x,y,w,h in physical virtual-desktop pixels
winshot --all                         # every display, one file each (<name>-DISPLAY1.png ...)
winshot --from page.png               # process a saved PNG instead of capturing

Default output, sized for Claude (see "The default output" below):
      --long-edge N         long-edge cap, default 2000 (Claude Code's own cap)
      --tier high|standard  Claude resolution tier to size for; high = Claude 4.7 and later
      --max-detail          keep images that already fit at full size (more tokens)
      --text-scale S        skip measuring; use this text-safe scale

winshot --raw                         # lossless, full resolution, one file: for people
      --scale off|auto|N    --raw only: resampling (auto = calibrated, else 1/DPI scale)
      --fit WxH             --raw only: downscale to fit a box, aspect preserved

  -o, --out PATH            output path; WSL or Windows paths both work
      --format png|webp     default from -o's extension, else png; webp is lossless
      --filter F            lanczos3 (default; exact 1/2, 1/4 use area averaging) | mitchell | box | bilinear
      --flash               white flash over what was captured (default off)
      --shutter             camera-shutter click (default off)
      --delay MS            wait before capturing
      --cursor              include the mouse cursor (default off)
      --stdout              write the image to stdout (only when the result is one image)
  -v, --verbose             resolution, DPI, colour path, sizes, timings on stderr

winshot fx [--display N]              # preview the flash and shutter sound, capturing nothing
      --no-flash | --no-sound | --wav FILE (save the sound) | --duration MS (hold the flash)

winshot calibrate [--display N]       # OCR-measure this display's safe downscale, cache it for --scale auto
winshot calibrate --from shot.png     # same, on a saved image (prints only)
```

Minimized windows are restored without taking focus, captured, and re-minimized. When
several windows match, the topmost one with real content is taken: apps like CapCut
keep visible-but-blank helper windows above their main window.

---

## Measured on the target machine

Whole virtual desktop (5760x2711), wall time as launched from WSL, 3 runs each:

| | resolution | wall time |
|---|---|---|
| v0, original | 3456x2169 (logical, wrong) | 515-546 ms |
| v0, DPI-fixed (`legacy/`) | 5760x2711 | 641-671 ms |
| **winshot** | **5760x2711** | **413-443 ms** |

About 1.5x faster than a *correct* v0, not the 5-10x originally estimated: v0 was never
the 1.5-2.5 s the design assumed. The win that matters is correctness. Inside winshot,
WGC capture (~150-250 ms including a 36-53 ms D3D device) dominates, not the encoder.

## Design decisions, briefly

| decision | choice | one-line reason |
|---|---|---|
| language | Rust, `x86_64-pc-windows-msvc` | no runtime startup; any Windows-native language would do |
| capture API | Windows.Graphics.Capture | only WGC does per-window *and* per-monitor, incl. occluded / GPU-composited windows |
| DPI | manifest + startup assertion + per-frame size check | the v0 failure was silent, so every layer fails loudly |
| default mode | **sized for Claude**; `--raw` for a lossless original | the tool is for LLMs; a plain capture loses text on the way to Claude |
| file format | **PNG**, zlib "balanced" | lossless, universally pasteable; 2x smaller than "fast" for ~50 ms per 9 Mpx |
| palette path | automatic, not the size story | <= 256 colours is **rare** in practice (0 of 8 real captures); costs ~1 ms to check |
| WebP | lossless only, opt-in | measured *larger* than balanced PNG on 5 of 8 captures |
| JPEG | not offered | 8x8 DCT rings around glyph edges |
| `--raw` resampling | **off** unless `--scale`/`--fit` | resampling text is never free |
| goldilocks metric | OCR word recall, relative to OCR's own noise floor | the specified absolute 99.5% is unreachable with Windows OCR (see DESIGN §5) |
| flash + shutter | off by default, fired **after** capture | verified: the flash is on screen, never in the shot |
| LLM sizing | smallest text sets the scale; long images become tiles | a plain fit-to-2000 took a tall page's text to 2.9 px |

### On the "goldilocks" downscale

The binding constraint is **glyph height in pixels**, not image dimensions. With `--raw`, `--scale auto`
uses, per display, the value cached by `winshot calibrate`, else `1 / DPI scale` (0.5 at
200%, 0.8 at 125%). For a capture spanning mixed-DPI displays it takes the *least*
aggressive display's scale, so no display's text drops below its safe size.

`calibrate` uses the OCR engine built into Windows (no extra dependency), and scores only
the **smallest text size** on the screen, because an average lets a status bar vanish.
On synthetic text the line height that survives the calibrated scale is 9.6-10.3 px across
a 3x range of font sizes (`scripts/check-legibility.py sweep`). That is Windows OCR's limit;
Claude reads smaller (below), so `calibrate` is conservative for Claude.

### The default output (sized for Claude)

Claude Code shrinks every image to <= 2000 px on the long edge before Claude sees it
(observed: 3840x2400 arrives as 2000x1250), and Claude rejects anything over 8000 px. A plain
resize to fit shrinks text with the image, and on a tall page it destroys it. So by default
winshot (the older spelling `--for-claude` is still accepted):

1. OCRs the image and, per **letter shape** (lowercase text / CAPS / digits), finds the
   smallest text size with real content, estimating each line's real height from the
   letters in it (OCR boxes "added" at 78% of its real height, "mono" at 56%);
2. scales so every shape stays at **Claude's own measured threshold** for it: text 9 px,
   caps 7 px, digits 6 px (random words it cannot guess, 2 fonts, light and dark themes;
   DESIGN §5d);
3. keeps one image if fitting the long edge stays within 5% of that scale; otherwise keeps
   the text scale and **splits the long axis into balanced tiles**, cut in the blank row
   nearest each balance point (overlapping only where no blank row exists);
4. splits a multi-monitor desktop at monitor edges first, and names tiles `-1of4`, `-2of4`...

Measured: the 3840x2400 laptop screen becomes **one 1617x1008 image, 2,091 tokens**, and
Claude reads back the same 97.5% of its words as from 3,243- and 4,564-token versions. A
1800x17886 retina page becomes 5 tiles with its smallest text at 9 px (a plain fit: 1.9 px,
and the raw file is rejected outright). A walkthrough with every stage's
images is in the artifact linked from HANDOFF.md. Measuring costs 0.7 s on a screen and
2.2 s on that page; `--max-detail` skips it when the image already fits.

Every estimate is built to err toward more resolution: box ratios are the largest measured,
and anything that could stretch a box (brackets, commas, `j`, accented letters, unknown
marks) counts as full height. A code review found and fixed four ways the first version
could err the other way. `--text-scale` overrides the measurement.

**Token cost is exact.** Claude reads 28x28-px patches, one token each: `ceil(w/28) x
ceil(h/28)`, and resizes anything over its tier's cap (high tier: 2576 px, 4784 tokens).
`-v` prints the size Claude will see and the exact tokens for every image. File size and
format do not affect tokens at all. The default mode saves tokens only in ways that keep text:
it never exceeds the cap (which would trigger a silent server-side shrink), crops uniform
margins, fills partial patches with free resolution, and shrinks images that already fit
down to their verified text-safe scale. `scripts/count-tokens.py` confirms the counts
against the API's `count_tokens` endpoint (exact on every image tested: patches + 3 per
image), and `scripts/claude-legibility.py` measured the threshold above (DESIGN §5c-5d).

---

## Build and install

Needs the Windows-side Rust toolchain (`x86_64-pc-windows-msvc`) and VS Build Tools;
everything is driven from WSL:

```bash
./install.sh          # build, copy winshot.exe to %LOCALAPPDATA%\winshot, shim to ~/.local/bin
./build.sh            # build only (target dir on NTFS: C:\Users\<you>\AppData\Local\winshot-build)
./build.sh test       # unit tests (palette, resampling, OCR metric, smallest text, tiling, shutter)
python3 scripts/check-legibility.py  # synthetic text of known size: sweep, mixed sizes, tall page
python3 scripts/check-fx.py          # flash visible on screen, absent from the shot; sound plays
python3 scripts/count-tokens.py *.png   # exact token cost per image from the API (needs a key; free)
python3 scripts/claude-legibility.py --dry-run   # Claude's own threshold; a real run ~$0.40 (estimate)
```

`install.sh` backs up an existing v0 `~/.local/bin/winshot` to `winshot.v0.bak`.

The shim translates absolute WSL paths (`-o`, `--from`) to Windows paths and maps the
printed result back. Relative paths need no help: a Windows process launched from WSL
inherits the `\\wsl.localhost\...` working directory, and the file is written in a single
sequential write (5-25 ms measured).

### v0 (still works, now at physical resolution)

```bash
install -m755 legacy/winshot.sh ~/.local/bin/winshot
cp legacy/winshot.ps1 /mnt/c/Users/$USER/AppData/Local/Temp/
```

---

## License

MIT
