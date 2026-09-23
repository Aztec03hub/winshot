# winshot

Fast screenshots of a **Windows** desktop, driven from **WSL**.

Enumerate displays and windows, capture one monitor / one window / one process / an
arbitrary region, and get a small file back without losing text legibility.

> **Status: design complete, implementation pending.**
> `legacy/` holds the working v0 (bash + PowerShell). The Rust rewrite is specified in
> [`docs/DESIGN.md`](docs/DESIGN.md) and briefed in [`HANDOFF.md`](HANDOFF.md).

---

## Why this exists

WSL has no access to the Windows desktop compositor, so a Linux binary **cannot** capture
the Windows screen no matter what language it is written in. Something Windows-native has
to do the capture. The v0 shelled out to PowerShell; that works but is slow and, it turns
out, silently wrong.

### The bug that motivated the rewrite

PowerShell's `System.Windows.Forms` is **DPI-unaware**. On this machine:

| display | physical | logical | scaling |
|---|---|---|---|
| DISPLAY1 | 3840x2400 | 1920x1200 | 200% |
| DISPLAY5 | 3072x1728 | 1536x864 | 200% |
| **virtual desktop** | **6912x4338** | **3456x2169** | |

The v0 captures the **logical** 3456x2169 — half the linear resolution, a **4x pixel
loss** — and never says so. Any DPI-scaled display has been quietly losing detail.

A correct implementation must declare `PER_MONITOR_AWARE_V2` before touching any
capture API.

---

## CLI surface

```
winshot list displays                 # index, name, physical+logical res, DPI, position, HDR
winshot list windows [--filter STR]   # hwnd, pid, exe, title, monitor, state

winshot                               # whole virtual desktop -> ./winshot-<ts>.png
winshot --display 1                   # one monitor by index
winshot --display primary
winshot --window "CapCut"             # by title substring
winshot --pid 27768                   # by process id
winshot --exe capcut.exe              # by executable name
winshot --region 100,200,800,600      # x,y,w,h in physical pixels
winshot --all                         # every display, one file each

  -o, --out PATH            output path (default ./winshot-<ts>.png)
      --format png|webp     default png
      --scale auto|N|off    resampling; default off
      --fit WxH             downscale to fit a box, aspect preserved
      --flash               white flash on the captured display (default off)
      --shutter             camera-shutter sound (default off)
      --delay MS            wait before capturing
      --cursor              include the mouse cursor (default off)
      --stdout              write the image to stdout instead of a file

winshot calibrate [--display N]       # find this display's safe downscale threshold
```

---

## Design decisions, briefly

Full reasoning, including the measurements behind each, is in
[`docs/DESIGN.md`](docs/DESIGN.md).

| decision | choice | one-line reason |
|---|---|---|
| language | Rust, `x86_64-pc-windows-msvc` | kills ~600 ms of PowerShell startup; the exe must be Windows-native regardless |
| capture API | Windows.Graphics.Capture, GDI `PrintWindow` fallback | only WGC does per-window *and* per-monitor, and handles occluded/GPU-composited windows |
| default format | **PNG** | lossless, and the only format that pastes reliably into GitHub, Slack, and Claude |
| default size win | **lossless palette reduction** | UI screenshots often have <256 colors; indexing cuts 50-75% with zero pixel change |
| default resampling | **off** | resampling text is never free; the lossless win comes first |
| goldilocks metric | **OCR agreement**, not SSIM/PSNR | pixel-fidelity metrics do not measure whether a glyph is still readable |
| flash + shutter | off by default, fired **after** capture | otherwise the flash lands in the screenshot |

### On file format

JPEG is disqualified outright: its 8x8 DCT produces ringing around glyph edges, which is
the exact failure mode that matters for screenshots of text. PNG is the default because
it is lossless and universally pasteable. `--format webp` (lossless mode) is offered for
roughly 25-35% smaller files when the destination is known to support it.

### On the "goldilocks" downscale

The binding constraint is **glyph height in pixels**, not image dimensions. A fixed ratio
is wrong: 0.5x on a 200%-scaled 4K panel is comfortable, while 0.5x on a 1080p display
with 12 px text destroys it.

Three signals, cheapest first:

1. **DPI-derived** (free) — at 200% scaling, `scale = 0.5` reproduces what a 100% display
   shows. Empirically validated on this machine: a 0.5-scale capture of a dense terminal
   was fully legible.
2. **Measured glyph height** — estimate text line pitch from row-wise edge density, pick
   the scale that keeps x-height above ~7 px.
3. **OCR-calibrated** (`winshot calibrate`) — binary-search the scale, OCR at each step,
   and take the smallest scale holding >=99.5% character agreement against the full-res
   reference. Slow, so it runs once per display and caches the result.

---

## Install

```bash
# from WSL, once the Rust build exists
cargo build --release --target x86_64-pc-windows-msvc
install -m755 target/x86_64-pc-windows-msvc/release/winshot.exe ~/.local/bin/
install -m755 shim/winshot ~/.local/bin/winshot
```

The shim translates WSL paths to Windows paths and back, so `-o ./shot.png` works from
any WSL directory.

### v0 (works today)

```bash
install -m755 legacy/winshot.sh ~/.local/bin/winshot
cp legacy/winshot.ps1 /mnt/c/Users/$USER/AppData/Local/Temp/
```

Captures at logical resolution only — see the bug above.

---

## License

MIT
