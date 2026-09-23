#!/usr/bin/env python3
"""Falsifiable checks of winshot's text-legibility logic on synthetic images of
KNOWN glyph size (the ground truth OCR cannot fake). Run from WSL after ./install.sh:

    python3 scripts/check-legibility.py            # all checks
    python3 scripts/check-legibility.py sweep      # just the SAFE_LINE_PX derivation

1. sweep  - `calibrate` on one text size at a time: the OCR line height that survives
            the calibrated scale must be ~constant across a 3x range of font sizes.
            (~10 px: Windows OCR's own threshold. Claude's, from
            scripts/claude-legibility.py, is lower: SAFE_LINE_PX = 8.)
2. mixed  - body text plus a small minority status bar: the status bar, not the
            average, must set the scale. Removing the bar must let the scale drop.
3. tall   - a 14pt-on-retina page 16000 px tall through the default (LLM) mode: every tile
            within the long edge, text never shrunk below legible, seams between lines.

Needs Pillow and the Windows Consolas font.
"""
import os, random, re, statistics, subprocess, sys, tempfile
from PIL import Image, ImageDraw, ImageFont

USER = os.environ["USER"]
EXE = os.environ.get("WINSHOT_EXE", f"/mnt/c/Users/{USER}/AppData/Local/winshot/winshot.exe")
FONT = "/mnt/c/Windows/Fonts/consola.ttf"
BG, FG = (30, 30, 46), (205, 214, 244)
random.seed(7)
WORDS = ["".join(random.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(random.randint(3, 9))) for _ in range(3000)]


def win(p):
    return subprocess.check_output(["wslpath", "-w", p], text=True).strip()


def draw_block(d, font, px, x0, y0, x1, y1, words):
    """Wrap words into [x0,x1) x [y0,y1); returns the next unused word index."""
    y, i = y0, 0
    while y + px * 1.4 <= y1 and i < len(words):
        line = ""
        while i < len(words) and font.getlength(line + words[i] + " ") < x1 - x0:
            line += words[i] + " "
            i += 1
        d.text((x0, y), line, font=font, fill=FG)
        y += int(px * 1.4)
    return i


def grab(pattern, text):
    m = re.search(pattern, text)
    if not m:
        sys.exit(f"expected /{pattern}/ in winshot output:\n{text}")
    return float(m.group(1))


def run(args):
    r = subprocess.run([EXE, *args], capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"winshot {' '.join(args)} failed:\n{r.stdout}{r.stderr}")
    return r.stdout, r.stderr


def sweep(tmp):
    rows = []
    for px in (16, 24, 32, 48):
        img = Image.new("RGB", (1800, 1200), BG)
        draw_block(ImageDraw.Draw(img), ImageFont.truetype(FONT, px), px, 10, 10, 1790, 1190, WORDS)
        p = os.path.join(tmp, f"sweep{px}.png")
        img.save(p)
        out, err = run(["calibrate", "--from", win(p)])
        s, h = grab(r"safe scale ([0-9.]+)", out), grab(r"smallest text: ([0-9.]+) px", err)
        rows.append((px, h, s, h * s))
        print(f"  font {px:>2}px  OCR line {h:>3.0f}px  calibrated x{s:.3f}  surviving line {h * s:4.1f}px")
    # 48px hits the 0.25 search floor: its surviving height is an upper bound, not a measurement.
    measured = [r[3] for r in rows if r[2] > 0.25]
    safe = statistics.median(measured)
    assert max(measured) / min(measured) < 1.6, f"surviving line height should be ~constant, got {measured}"
    print(f"OK sweep: OCR's surviving line {min(measured):.1f}-{max(measured):.1f}px, median {safe:.1f}px")


def for_claude(p, long_edge):
    out, err = run(["--from", win(p), "--long-edge", str(long_edge), "-v", "-o", win(p[:-4] + "-fc.png")])
    scale = grab(r"output \d+ image\(s\) at x([0-9.]+)", err)
    h = grab(r"smallest text ([0-9.]+) px", err)
    safe = grab(r"text-safe x([0-9.]+)", err)
    tiles = [tuple(map(int, t)) for t in re.findall(r"\((\d+)x(\d+)\)", out)]
    return scale, h, tiles, err, safe


def mixed(tmp):
    # A 16:10 screen, so the short edge always fits and only the text can set the scale.
    body, small = ImageFont.truetype(FONT, 36), ImageFont.truetype(FONT, 18)
    img = Image.new("RGB", (3200, 2000), BG)
    draw_block(ImageDraw.Draw(img), body, 36, 20, 20, 3180, 1930, WORDS)
    with_bar = img.copy()
    db = ImageDraw.Draw(with_bar)
    db.rectangle((0, 1940, 3200, 2000), fill=(20, 20, 30))
    db.text((20, 1958), "main*  3 errors  14 warnings  Ln 212, Col 9  UTF-8  LF  Rust  winshot  build ok  tests 13 passed", font=small, fill=FG)
    pa, pb = os.path.join(tmp, "mixed-bar.png"), os.path.join(tmp, "mixed-nobar.png")
    with_bar.save(pa)
    img.save(pb)
    # Long edge 1400: tighter than both images' text needs, so the text scale decides.
    sa, ha, ta, ea, safe_a = for_claude(pa, 1400)
    sb, hb, tb, eb, safe_b = for_claude(pb, 1400)
    print(f"  36px body + 18px status bar: smallest {ha:.0f}px, text-safe x{safe_a:.3f} -> {len(ta)} image(s) at x{sa:.3f}, bar at {ha * sa:.1f}px")
    print(f"  36px body only:              smallest {hb:.0f}px, text-safe x{safe_b:.3f} -> {len(tb)} image(s) at x{sb:.3f}")
    assert "even the short edge" not in ea + eb, "test geometry must not be short-edge limited"
    assert ha < hb * 0.7, "the status bar must be detected as the smallest text"
    assert ha * sa >= 8.9, f"the status bar (lowercase, full-height line) must stay at the 9 px text threshold, got {ha * sa:.1f}px"
    assert safe_a > safe_b * 1.3, "the status bar, a small minority of the words, must raise the text-safe scale"
    print("OK mixed: the minority small text sets the scale; without it the scale drops")


def tall(tmp):
    # 14pt on a 2x display is ~28 px: the case from the Claude vision docs discussion.
    px = 28
    font = ImageFont.truetype(FONT, px)
    img = Image.new("RGB", (1600, 16000), BG)
    d = ImageDraw.Draw(img)
    y, i = 20, 0
    while y < 16000 - 200:
        n = random.randint(4, 12)  # paragraphs, with gaps between them
        i += draw_block(d, font, px, 30, y, 1570, y + int(n * px * 1.4), WORDS[i % 2000:])
        y += int(n * px * 1.4) + px
    p = os.path.join(tmp, "tall.png")
    img.save(p)
    s, h, tiles, err, _ = for_claude(p, 2000)
    naive = 2000 / 16000
    print(f"  1600x16000 page, smallest text {h:.0f}px: x{s:.3f} -> {len(tiles)} tiles {tiles[0]}..{tiles[-1]}")
    print(f"  a plain fit-to-2000 resize would be x{naive:.3f}: text {h * naive:.1f}px")
    assert all(max(t) <= 2000 for t in tiles), "every tile within the long edge"
    assert h * s >= 8.9, f"text must stay at the 9 px text threshold, got {h * s:.1f}px"
    overlapped = int(grab(r"(\d+) overlapped", err))
    assert overlapped == 0, f"paragraph gaps exist, so every seam should be blank; {overlapped} overlapped"
    print(f"OK tall: {len(tiles)} tiles, text {h * s:.1f}px instead of {h * naive:.1f}px, every seam between lines")


if __name__ == "__main__":
    want = sys.argv[1:] or ["sweep", "mixed", "tall"]
    with tempfile.TemporaryDirectory(dir=f"/mnt/c/Users/{USER}/AppData/Local/Temp") as tmp:
        for name in want:
            print(f"== {name}")
            globals()[name](tmp)
