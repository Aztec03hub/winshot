#!/usr/bin/env python3
"""Ask the Claude API what winshot's output images really cost, and prove it would
not resize them.

    pip install anthropic       # once
    python3 scripts/count-tokens.py shot-1of2.png shot-2of2.png [--model claude-opus-5-5]

For each PNG this calls the free /v1/messages/count_tokens endpoint twice (with and
without the image) so the difference is the image's own token cost, and marks the
image `"oversized_image": "error"`: the endpoint then rejects any image the API
would downscale, instead of silently shrinking it. It prints the API's figure next
to winshot's (ceil(w/28) x ceil(h/28) patches + 3 per image block, what winshot -v
reports; the +3 was measured with this script) and fails if they disagree or any
image would be resized.

Credentials: ANTHROPIC_API_KEY, or a profile from `ant auth login`.
"""
import argparse, base64, math, struct, sys

import anthropic


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    if head[:8] != b"\x89PNG\r\n\x1a\n":
        sys.exit(f"{path}: not a PNG")
    return struct.unpack(">II", head[16:24])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("images", nargs="+")
    ap.add_argument("--model", default="claude-opus-5-5", help="model whose tier and tokenizer to count for")
    a = ap.parse_args()
    client = anthropic.Anthropic()
    label = [{"type": "text", "text": "image"}]
    base = client.messages.count_tokens(model=a.model, messages=[{"role": "user", "content": label}]).input_tokens
    ok, total = True, 0
    for path in a.images:
        w, h = png_size(path)
        block = {
            "type": "image",
            "source": {"type": "base64", "media_type": "image/png", "data": base64.b64encode(open(path, "rb").read()).decode()},
            "transformations": {"oversized_image": "error"},
        }
        try:
            n = client.messages.count_tokens(model=a.model, messages=[{"role": "user", "content": [block, *label]}]).input_tokens - base
        except anthropic.BadRequestError as e:
            print(f"FAIL {path} ({w}x{h}): the API would resize it: {e.message}")
            ok = False
            continue
        local = math.ceil(w / 28) * math.ceil(h / 28) + 3  # patches + per-image-block overhead
        mark = "ok  " if n == local else "DIFF"
        ok &= n == local
        total += n
        print(f"{mark} {path}: {w}x{h}, API {n} tokens, winshot {local}")
    print(f"total {total} tokens for {len(a.images)} image(s) on {a.model}")
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
