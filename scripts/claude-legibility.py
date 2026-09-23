#!/usr/bin/env python3
"""Measure Claude's OWN text-legibility threshold. SAFE_LINE_PX in src/calibrate.rs
(8 px, DESIGN §5d), which sizes winshot's default output, comes from this script.

Renders lines of random words (so nothing can be guessed from context) in several
fonts and letter shapes (lowercase, Mixed Case, ALL CAPS, digits), downsizes each image so the
OCR line height lands at a set of target heights, sends it to Claude, and scores
word recall against the ground truth. The threshold is the smallest height whose
recall stays within 90% of the full-size recall, per letter shape.

    pip install anthropic pillow
    python3 scripts/claude-legibility.py --dry-run     # images + cost estimate, no API calls
    python3 scripts/claude-legibility.py               # SPENDS MONEY: see the estimate first

Cost (from --dry-run): 48 calls, ~7.7k image tokens and ~19k output tokens; on
claude-opus-5-5 ($4 / $20 per MTok) about $0.40. Needs Windows fonts under /mnt/c.
"""
import argparse, base64, io, math, os, random, re, tempfile, zlib
from PIL import Image, ImageDraw, ImageFont

FONTS = {"consolas": "/mnt/c/Windows/Fonts/consola.ttf", "segoe": "/mnt/c/Windows/Fonts/segoeui.ttf"}
SHAPES = {"lower": str.lower, "mixed": str.capitalize, "caps": str.upper, "digits": None}
# Target line heights in px after downscaling; the OCR sweep put its threshold at ~10.
HEIGHTS = [5, 6, 7, 8, 9, 10, 12, 16]
RENDER_PX = 40  # render big, then downscale: the result is what a screenshot would carry


def words(shape, rng, n):
    if shape == "digits":
        return ["".join(rng.choice("0123456789") for _ in range(rng.randint(3, 7))) for _ in range(n)]
    out = ["".join(rng.choice("abcdefghijklmnopqrstuvwxyz") for _ in range(rng.randint(3, 8))) for _ in range(n)]
    return [SHAPES[shape](w) for w in out]


# (background, text): dark-on-light paper, and a terminal-style light-on-dark theme.
THEMES = {"light": ((255, 255, 255), (20, 20, 20)), "dark": ((30, 30, 46), (205, 214, 244))}


def render(font_path, shape, seed, theme="light"):
    rng = random.Random(seed)
    font = ImageFont.truetype(font_path, RENDER_PX)
    ws = words(shape, rng, 60)
    bg, fg = THEMES[theme]
    img = Image.new("RGB", (1400, 12 * int(RENDER_PX * 1.5)), bg)
    d = ImageDraw.Draw(img)
    y, i, lines = 10, 0, []
    while i < len(ws) and y + RENDER_PX * 1.5 < img.height:
        line = []
        while i < len(ws) and font.getlength(" ".join(line + [ws[i]])) < img.width - 20:
            line.append(ws[i])
            i += 1
        d.text((10, y), " ".join(line), font=font, fill=fg)
        lines.append(line)
        y += int(RENDER_PX * 1.5)
    # The rendered line box height (ascender to descender of what was drawn), the
    # quantity winshot's OCR measures as "line height".
    boxes = [font.getbbox(" ".join(l)) for l in lines]
    line_h = max(b[3] - b[1] for b in boxes)
    return img.crop((0, 0, img.width, y)), [w for l in lines for w in l], line_h


def recall(said, truth):
    want = {}
    for w in truth:
        want[w] = want.get(w, 0) + 1
    hit = 0
    for w in re.findall(r"[A-Za-z0-9]+", said):
        if want.get(w, 0) > 0:
            want[w] -= 1
            hit += 1
    return hit / len(truth)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", default="claude-opus-5-5")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--theme", choices=list(THEMES), default="light")
    ap.add_argument("--shapes", nargs="*", default=list(SHAPES), help=f"subset of {list(SHAPES)}")
    ap.add_argument("--fonts", nargs="*", default=list(FONTS), help=f"subset of {list(FONTS)}")
    ap.add_argument("--heights", nargs="*", type=int, default=None, help="subset of target heights (full size is always included)")
    ap.add_argument("--seed", default="", help="salt for the random words, e.g. to retry cells a classifier refused")
    a = ap.parse_args()
    cases = []
    for fname, fpath in ((f, FONTS[f]) for f in a.fonts):
        for shape in a.shapes:
            img, truth, line_h = render(fpath, shape, seed=zlib.crc32(f"{fname}/{shape}{a.seed}".encode()), theme=a.theme)
            for target in [None, *(a.heights or HEIGHTS)]:  # None = full size, the reference
                s = 1.0 if target is None else target / line_h
                im = img if target is None else img.resize((max(1, round(img.width * s)), max(1, round(img.height * s))), Image.LANCZOS)
                cases.append((fname, shape, target or round(line_h), im, truth))
    tok = sum(math.ceil(im.width / 28) * math.ceil(im.height / 28) for *_, im, _ in cases)
    print(f"{len(cases)} calls, ~{tok} image tokens + ~{len(cases) * 400} output tokens")
    if a.dry_run:
        out = os.path.join(tempfile.gettempdir(), "winshot-legibility")
        os.makedirs(out, exist_ok=True)
        for f, sh, h, im, _ in cases:
            im.save(os.path.join(out, f"{f}-{sh}-{h:02d}px.png"))
        print(f"dry run: wrote all {len(cases)} images to {out}, no API calls")
        return
    import anthropic

    client = anthropic.Anthropic()
    results = {}
    for fname, shape, h, im, truth in cases:
        buf = io.BytesIO()
        im.save(buf, "PNG")
        msg = client.messages.create(
            model=a.model,
            max_tokens=2000,
            output_config={"effort": "low"},
            messages=[{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64.b64encode(buf.getvalue()).decode()}},
                {"type": "text", "text": "Transcribe every word in this image exactly, in order, separated by spaces. The words are random strings, so do not correct or guess: if a word is unreadable, write ? for it. Output only the transcription."},
            ]}],
        )
        if msg.stop_reason == "refusal":
            print(f"{fname} {shape} {h}px: refused")
            continue
        said = "".join(b.text for b in msg.content if b.type == "text")
        r = recall(said, truth)
        results.setdefault((fname, shape), []).append((h, r))
        print(f"{fname:<9} {shape:<7} line {h:>3}px  recall {r:6.1%}")
    print("\nthreshold = smallest line height at which it AND every larger height read >= 90% of full size:")
    for (fname, shape), rows in results.items():
        full_rows = [r for h, r in rows if h not in HEIGHTS]
        if not full_rows:
            print(f"  {fname:<9} {shape:<7} no full-size reference (refused?): no threshold")
            continue
        full = full_rows[0]
        scaled = sorted((h, r) for h, r in rows if h in HEIGHTS)
        threshold = None
        for i, (h, _) in enumerate(scaled):
            if all(r >= 0.9 * full for _, r in scaled[i:]):
                threshold = h
                break
        print(f"  {fname:<9} {shape:<7} full {full:6.1%}  threshold {threshold if threshold else 'none'} px")


if __name__ == "__main__":
    main()
