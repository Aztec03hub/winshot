#!/usr/bin/env python3
"""Prove what a set of images costs and what Claude can still read in it.

    python3 scripts/fidelity-check.py --truth-text page.txt \\
        --variant tiles=p-1of3.png,p-2of3.png,p-3of3.png --variant plain=p-naive.png \\
        [--truth-images q1.png q2.png ...] [--probe WORD ...] [--json out.json]

For every image: the API's own token count (count_tokens, with the request id as
evidence), marked "oversized_image": "error" so a count also proves the API will not
resize it. For every variant: one real Messages call in which Claude transcribes all
of the variant's images, recording the tokens actually billed (usage) and the
transcription's recall of the ground-truth words.

Ground truth is either exact text (--truth-text: e.g. the source of a rendered page) or
Claude's own transcription of full-resolution reference images (--truth-images: e.g.
quarters of a capture, each small enough that the API does not resize it). --probe
words (e.g. the smallest text winshot measured) are reported found / missing per variant.

SPENDS MONEY (one call per variant, plus one per truth image); prints the exact spend.
Credentials: ANTHROPIC_API_KEY. Needs `pip install anthropic`.
"""
import argparse, base64, json, re, struct, sys

import anthropic

PRICE_IN, PRICE_OUT = 4.0, 20.0  # claude-opus-5-5, $ per MTok (skill model table, 2026-06-24)
PROMPT = (
    "Transcribe ALL visible text in these images, in reading order, image by image. "
    "Include small print, labels, menus, status bars and code exactly as written. "
    "Do not summarise, correct or guess; write ? for anything unreadable. Output only the transcription."
)


def png_size(path):
    with open(path, "rb") as f:
        head = f.read(24)
    return struct.unpack(">II", head[16:24])


def image_block(path, strict=True):
    b = {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64.b64encode(open(path, "rb").read()).decode()}}
    if strict:
        b["transformations"] = {"oversized_image": "error"}
    return b


def words(text):
    return [w.lower() for w in re.findall(r"[A-Za-z0-9]+", text)]


def recall(candidate, truth):
    want = {}
    for w in truth:
        want[w] = want.get(w, 0) + 1
    hit = 0
    for w in candidate:
        if want.get(w, 0) > 0:
            want[w] -= 1
            hit += 1
    missed = [w for w, n in want.items() for _ in range(n)]
    return hit / max(len(truth), 1), missed


def transcribe(client, model, paths, strict=True):
    content = []
    for i, p in enumerate(paths, 1):
        content += [{"type": "text", "text": f"Image {i}:"}, image_block(p, strict)]
    content.append({"type": "text", "text": PROMPT})
    with client.messages.stream(model=model, max_tokens=32000, output_config={"effort": "low"}, messages=[{"role": "user", "content": content}]) as s:
        msg = s.get_final_message()
        rid = s.response.headers.get("request-id")
    if msg.stop_reason == "refusal":
        return None, msg, rid
    return "".join(b.text for b in msg.content if b.type == "text"), msg, rid


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--truth-text")
    ap.add_argument("--truth-images", nargs="*", default=[])
    ap.add_argument("--variant", action="append", required=True, help="NAME=img1.png,img2.png")
    ap.add_argument("--probe", nargs="*", default=[])
    ap.add_argument("--model", default="claude-opus-5-5")
    ap.add_argument("--json")
    a = ap.parse_args()
    if not a.truth_text and not a.truth_images:
        ap.error("give a ground truth: --truth-text FILE or --truth-images IMG ...")
    client = anthropic.Anthropic()
    spent = {"input": 0, "output": 0}
    out = {"model": a.model, "variants": []}

    if a.truth_text:
        truth_text = open(a.truth_text).read()
        out["truth"] = {"kind": "exact text", "file": a.truth_text}
    else:
        text, msg, rid = transcribe(client, a.model, a.truth_images)
        if text is None:
            sys.exit("the truth transcription was refused")
        spent["input"] += msg.usage.input_tokens
        spent["output"] += msg.usage.output_tokens
        truth_text = text
        out["truth"] = {"kind": "Claude reading full-resolution references", "images": a.truth_images, "request_id": rid}
    truth = words(truth_text)
    out["truth"]["words"] = len(truth)
    print(f"truth: {len(truth)} words ({out['truth']['kind']})")

    base = client.messages.count_tokens(model=a.model, messages=[{"role": "user", "content": [{"type": "text", "text": "x"}]}]).input_tokens
    for spec in a.variant:
        name, paths = spec.split("=", 1)
        paths = paths.split(",")
        v = {"name": name, "images": []}
        for p in paths:
            w, h = png_size(p)
            raw = client.messages.with_raw_response.count_tokens(model=a.model, messages=[{"role": "user", "content": [image_block(p), {"type": "text", "text": "x"}]}])
            n = raw.parse().input_tokens - base
            v["images"].append({"file": p.rsplit("/", 1)[-1], "w": w, "h": h, "api_tokens": n, "count_request_id": raw.headers.get("request-id")})
        text, msg, rid = transcribe(client, a.model, paths)
        spent["input"] += msg.usage.input_tokens
        spent["output"] += msg.usage.output_tokens
        v["billed_input_tokens"] = msg.usage.input_tokens
        v["request_id"] = rid
        if text is None:
            v["refused"] = True
            print(f"{name}: refused")
            out["variants"].append(v)
            continue
        r, missed = recall(words(text), truth)
        got = set(words(text))
        v.update(recall=r, missed_sample=missed[:40], missed_count=len(missed), transcription=text,
                 probes={p: p.lower() in got for p in a.probe})
        found = sum(v["probes"].values())
        print(f"{name}: {sum(i['api_tokens'] for i in v['images'])} image tokens (API), recall {r:.1%}, "
              f"probes {found}/{len(a.probe)}, billed input {msg.usage.input_tokens}")
        out["variants"].append(v)

    cost = spent["input"] / 1e6 * PRICE_IN + spent["output"] / 1e6 * PRICE_OUT
    out["spent"] = {**spent, "usd": round(cost, 4)}
    print(f"spent: {spent['input']} input + {spent['output']} output tokens = ${cost:.3f}")
    if a.json:
        json.dump(out, open(a.json, "w"), indent=1)


if __name__ == "__main__":
    main()
