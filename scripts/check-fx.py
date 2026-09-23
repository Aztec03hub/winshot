#!/usr/bin/env python3
"""Check the capture flash and shutter sound on the real desktop. Run from WSL:

    python3 scripts/check-fx.py

1. shutter  - `winshot fx --no-flash` must report that Windows played the sound, and the
              synthesized .wav must be well-formed (44.1 kHz mono 16-bit, ~140 ms).
2. visible  - a second capture taken while `winshot fx` holds a long flash must be much
              brighter than a baseline: the flash really reaches the screen.
3. absent   - a `--flash` capture must match a plain capture taken just before it: the
              flash fires only after the frame is grabbed, so it is never in the shot.
4. in-shot  - a real `--shutter` capture must not warn that the sound failed.

You will see the display flash and hear the click. Needs Pillow; uses DISPLAY1.
"""
import os, subprocess, sys, tempfile, time, wave
from PIL import Image, ImageStat

USER = os.environ["USER"]
EXE = os.environ.get("WINSHOT_EXE", f"/mnt/c/Users/{USER}/AppData/Local/winshot/winshot.exe")
TMP = tempfile.mkdtemp(dir=f"/mnt/c/Users/{USER}/AppData/Local/Temp")
win = lambda p: subprocess.check_output(["wslpath", "-w", p], text=True).strip()


def run(*args):
    r = subprocess.run([EXE, *args], capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"winshot {' '.join(args)} failed:\n{r.stdout}{r.stderr}")
    return r.stdout + r.stderr


def luma(name):
    return ImageStat.Stat(Image.open(os.path.join(TMP, name)).convert("L")).mean[0]


def shot(name, *extra):
    # --raw: one lossless file, so brightness compares like with like.
    return run("--raw", "--display", "1", "-o", win(os.path.join(TMP, name)), *extra)


print("== shutter")
wav = os.path.join(TMP, "shutter.wav")
out = run("fx", "--no-flash", "--wav", win(wav))
assert "shutter: played" in out, out
with wave.open(wav) as w:
    secs = w.getnframes() / w.getframerate()
    assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (44100, 1, 2), "format"
    assert 0.13 <= secs <= 0.15, f"{secs:.3f} s"
print(f"OK shutter: Windows reported it played; wav is 44.1 kHz mono 16-bit, {secs * 1000:.0f} ms ({wav})")

print("== flash visible on screen")
shot("base.png")
fx = subprocess.Popen([EXE, "fx", "--no-sound", "--display", "1", "--duration", "2000"], stdout=subprocess.PIPE, text=True)
time.sleep(0.6)  # well inside the 2 s fade: alpha ~0.7 x 180
shot("during.png")
fx.wait()
b, d = luma("base.png"), luma("during.png")
# A white overlay at alpha a moves each pixel a of the way to white, so measure the
# rise as a fraction of the headroom: a fixed luma delta fails on a bright screen.
if 255 - b < 8:
    sys.exit(f"DISPLAY1 is almost white (luma {b:.1f}): a white flash cannot be measured; retry with darker content")
rise = (d - b) / (255 - b)
assert rise > 0.25, f"no visible flash: luma {b:.1f} -> {d:.1f} ({rise:.0%} of the way to white)"
print(f"OK visible: mean luma {b:.1f} before, {d:.1f} mid-flash ({rise:.0%} of the way to white)")

print("== flash absent from its own shot")
shot("plain.png")
shot("flashed.png", "--flash")
p, f = luma("plain.png"), luma("flashed.png")
leak = (f - p) / max(255 - p, 1)
assert leak < 0.05, f"the flash leaked into the shot: luma {p:.1f} -> {f:.1f} ({leak:.0%} toward white)"
print(f"OK absent: plain {p:.1f}, --flash shot {f:.1f} ({leak:+.1%} toward white; a flash in the frame would be ~{rise:.0%})")

print("== shutter on a real capture")
err = shot("click.png", "--shutter")
assert "did not play" not in err, err
print("OK in-shot: --shutter capture played without a warning")
print(f"all fx checks passed (files in {TMP})")
