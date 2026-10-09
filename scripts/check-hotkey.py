#!/usr/bin/env python3
"""Check `winshot hotkey` on the real desktop. Run from WSL:

    python3 scripts/check-hotkey.py

It starts the hotkey on a scratch folder, presses the combination with injected key events
(the same path a real keyboard takes), and checks, for ONE press:

1. file       - a new PNG named <date>_<time>_<ms>[-<window title>].png (or tiles, -1of3 ...)
2. clipboard  - holds the file's Windows path as text and the file itself
3. image      - one file: the clipboard bitmap has its size and is not blank; tiles: the
                clipboard has every path in order and no image (a clipboard holds one)
4. unique     - three quick presses give three different names
5. stop       - `--stop` ends it, and a second start works

By default each "press" is the hotkey MESSAGE posted to the daemon's window: everything after the
key is tested (capture of the focused display, files, clipboard), and no keystroke is sent to
whatever has focus. `--inject` injects the real key chord instead, which also tests the
RegisterHotKey binding; it goes to the focused window, so focus a local window first (a live
remote desktop session swallows it, measured 2026-10-09). The check overwrites your clipboard and
refuses to run while your own hotkey is running (stop it first with `winshot hotkey --stop`,
restart it after). Uses ctrl+F24 by default (nothing binds it). Needs Pillow.
"""
import argparse, os, re, shutil, subprocess, sys, tempfile, time
from PIL import Image

ap = argparse.ArgumentParser()
ap.add_argument("--key", default="ctrl+f24", help="combination to register and press (ctrl, alt, shift + F1-F24)")
ap.add_argument("--inject", action="store_true", help="inject the real key chord instead of posting the hotkey message (needs a local window in the foreground)")
args = ap.parse_args()
VK = {"ctrl": 0x11, "alt": 0x12, "shift": 0x10}
chord = [VK[p] if p in VK else 0x6F + int(p[1:]) for p in args.key.lower().split("+")]

USER = os.environ["USER"]
EXE = os.environ.get("WINSHOT_EXE", f"/mnt/c/Users/{USER}/AppData/Local/winshot/winshot.exe")
TMP = tempfile.mkdtemp(dir=f"/mnt/c/Users/{USER}/AppData/Local/Temp", prefix="hk-check-")
win = lambda p: subprocess.check_output(["wslpath", "-w", p], text=True).strip()
# UTF-8 both ways: a window title can hold characters outside the console code page (a bullet in
# an editor's "unsaved" marker), and PowerShell would print those back as control characters.
def ps(*a):
    """powershell.exe from WSL. Starting it while the machine is busy (right after a burst of
    screenshots) sometimes fails with a WSL interop timeout ("UtilAcceptVsock ... accept4 failed
    110") before running anything; that is not a result, so retry it, a few times at most."""
    attempt = 0
    while True:
        r = subprocess.run(["powershell.exe", "-NoProfile", "-Sta", *a], capture_output=True, text=True, encoding="utf-8")
        if "UtilAcceptVsock" not in r.stderr or attempt == 3:
            return r
        attempt += 1
        time.sleep(attempt)


def run(*a):
    attempt = 0
    while True:
        r = subprocess.run([EXE, *a], capture_output=True, text=True, timeout=60)
        # The same WSL interop timeout as in ps(): the exe never ran, so ask again.
        if "UtilAcceptVsock" not in r.stderr or attempt == 3:
            return r.returncode, (r.stdout + r.stderr).strip()
        attempt += 1
        time.sleep(attempt)


def press():
    """One press. Default: post WM_HOTKEY to the daemon's window, which runs everything after the
    key (capture, files, clipboard) without sending a single keystroke to whatever has focus.
    With --inject: inject the real key chord, which also tests the RegisterHotKey binding, but the
    chord goes to the focused window, and a live remote desktop session never lets it through."""
    if not args.inject:
        r = ps("-Command", "Add-Type -Namespace K -Name N -MemberDefinition '"
           '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr FindWindowEx(System.IntPtr p, System.IntPtr a, string c, string t);'
           '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr h, uint m, System.IntPtr w, System.IntPtr l);'
           "'; $h = [K.N]::FindWindowEx([IntPtr](-3), [IntPtr]::Zero, 'winshot_hotkey', $null); if ($h -eq [IntPtr]::Zero) { exit 3 }; [void][K.N]::PostMessage($h, 0x312, [IntPtr]1, [IntPtr]::Zero)")
        assert r.returncode == 0, f"the daemon's window was not found (powershell exit {r.returncode}): {r.stderr.strip()}"
        return
    down = "; ".join(f"[K.N]::keybd_event({k},0,0,[UIntPtr]::Zero); Start-Sleep -Milliseconds 30" for k in chord)
    up = "; ".join(f"[K.N]::keybd_event({k},0,2,[UIntPtr]::Zero); Start-Sleep -Milliseconds 30" for k in reversed(chord))
    ps("-Command", "Add-Type -Namespace K -Name N -MemberDefinition '[System.Runtime.InteropServices.DllImport(\"user32.dll\")] "
       f"public static extern void keybd_event(byte b, byte s, uint f, System.UIntPtr e);'; {down}; {up}")


def pngs():
    """Sorted by name, with tiles in numeric order (-2of10 before -10of10)."""
    return sorted((f for f in os.listdir(TMP) if f.endswith(".png")), key=lambda f: (re.sub(r"-\d+of\d+\.png$|\.png$", "", f), int((re.search(r"-(\d+)of\d+\.png$", f) or [0, 0])[1])))


def stems():
    """One entry per press: a press that came out as tiles has several files, one stem."""
    return sorted({re.sub(r"(-\d+of\d+)?\.png$", "", f) for f in pngs()})


def wait_for(n, secs=45):
    """Wait for n presses' worth of files."""
    end = time.time() + secs
    while time.time() < end:
        if len(stems()) >= n:
            time.sleep(1.0)  # tiles are written one by one, then the clipboard is set
            return
        time.sleep(0.1)
    sys.exit(f"FAIL: expected {n} press(es) within {secs} s, found {pngs()}")


CLIP = r"""
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Windows.Forms, System.Drawing
$d = [Windows.Forms.Clipboard]::GetDataObject()
'formats=' + ($d.GetFormats($false) -join ',')
'text=' + ([Windows.Forms.Clipboard]::GetText() -replace "`r?`n", '<NL>')
'drop=' + ([Windows.Forms.Clipboard]::GetFileDropList() -join '|')
$i = [Windows.Forms.Clipboard]::GetImage()
if ($i) { 'image={0}x{1}' -f $i.Width, $i.Height
  $b = New-Object Drawing.Bitmap $i; $g = New-Object Collections.Generic.List[int]
  foreach ($y in 0..15) { foreach ($x in 0..15) { $g.Add($b.GetPixel([int]($x * ($b.Width - 1) / 15), [int]($y * ($b.Height - 1) / 15)).ToArgb()) } }
  'grid=' + ($g -join ',')
  'pnglen=' + $d.GetData('PNG').Length }
"""

ok = False
if run("hotkey", "--status")[1] == "running":
    sys.exit("a hotkey is already running; stop it first (winshot hotkey --stop), run this check, then start it again")
try:
    print(f"== start ({args.key}, folder {TMP})")
    rc, out = run("hotkey", "--key", args.key, "--dir", win(TMP), "--no-sound", "--no-flash")
    assert rc == 0, out
    assert run("hotkey", "--status")[1] == "running"
    rc, out = run("hotkey", "--key", args.key, "--dir", win(TMP))
    assert rc == 0 and "already running" in out, f"second start: {out}"

    print("== one press")
    press()
    wait_for(1)
    files = pngs()
    assert re.match(r"^\d{4}-\d\d-\d\d_\d\d_\d\d_\d\d_\d{3}(-.+)?$", stems()[0]), f"file name {files[0]!r}"
    wpaths = [win(os.path.join(TMP, f)) for f in files]
    # The file is written a moment BEFORE the clipboard is set (the worker does both, in order), and
    # on a busy machine that moment can be long: wait for the clipboard to say it is this press's.
    end = time.time() + 30
    while True:
        clip = dict(l.split("=", 1) for l in ps("-Command", CLIP).stdout.replace("\r", "").splitlines() if "=" in l)
        if clip.get("text", "").split("<NL>") == wpaths or time.time() > end:
            break
        time.sleep(0.5)
    assert clip["text"].split("<NL>") == wpaths, f"clipboard text {clip['text']!r} != {wpaths!r}"
    assert clip["drop"].split("|") == wpaths, f"clipboard files {clip['drop']!r}"
    if len(files) == 1:
        img = Image.open(os.path.join(TMP, files[0]))
        assert clip["image"] == f"{img.size[0]}x{img.size[1]}", f"clipboard image {clip.get('image')} vs file {img.size}"
        # The same 16x16 grid of pixels from the file and from the clipboard bitmap: catches a
        # flipped DIB, swapped red and blue, or a different picture of the same size.
        (w, h), px = img.size, img.convert("RGB").tobytes()
        want = []
        for y in range(16):
            for x in range(16):
                i = (round(y * (h - 1) / 15) * w + round(x * (w - 1) / 15)) * 3  # round() is half-to-even, like PowerShell's [int]
                v = 0xFF000000 | px[i] << 16 | px[i + 1] << 8 | px[i + 2]
                want.append(v - (1 << 32) if v >= 1 << 31 else v)
        assert [int(v) for v in clip["grid"].split(",")] == want, "clipboard bitmap differs from the saved file"
        assert len(set(want)) > 8, "the screenshot is blank, so this comparison proves little"
        assert int(clip["pnglen"]) == os.path.getsize(os.path.join(TMP, files[0])), "the clipboard PNG is not the saved file"
        assert "PNG" in clip["formats"] and "DeviceIndependentBitmap" in clip["formats"], clip["formats"]
        print(f"OK file + clipboard: {files[0]} ({w}x{h}); text and file list are its path; bitmap pixels and PNG bytes equal the file")
    else:
        # Several tiles: a clipboard holds one image, so it carries the paths and no picture.
        assert re.search(r"-1of%d\.png$" % len(files), files[0]) and "image" not in clip, f"tiles {files}, clipboard {clip}"
        print(f"OK tiles: {len(files)} files named -NofM, clipboard has their paths and no image")

    print("== three quick presses")
    for _ in range(3):
        press()
        time.sleep(0.5)
    wait_for(4)
    assert len(stems()) == 4, pngs()
    print(f"OK unique: {len(stems())} presses, {len(pngs())} files, no name reused")

    print("== a burst of presses (frames of a video)")
    before = set(stems())

    def burst(n, gap):
        """n presses, gap seconds apart, posted in ONE powershell so the spacing is real."""
        post = ("$h = [K.N]::FindWindowEx([IntPtr](-3), [IntPtr]::Zero, 'winshot_hotkey', $null); if ($h -eq [IntPtr]::Zero) { exit 3 }; "
                f"1..{n} | ForEach-Object {{ [void][K.N]::PostMessage($h, 0x312, [IntPtr]1, [IntPtr]::Zero); Start-Sleep -Milliseconds {int(gap * 1000)} }}")
        r = ps("-Command", "Add-Type -Namespace K -Name N -MemberDefinition '"
               '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern System.IntPtr FindWindowEx(System.IntPtr p, System.IntPtr a, string c, string t);'
               '[System.Runtime.InteropServices.DllImport("user32.dll")] public static extern bool PostMessage(System.IntPtr h, uint m, System.IntPtr w, System.IntPtr l);'
               f"'; {post}")
        assert r.returncode == 0, f"daemon window not found: {r.stderr.strip()}"

    def ms(stem):  # the grab time in the name: hh_mm_ss_mmm, as ms since midnight
        found = re.search(r"_(\d\d)_(\d\d)_(\d\d)_(\d{3})", stem)
        assert found, f"no grab time in {stem!r}"
        h, m, s_, f = (int(x) for x in found.groups())
        return ((h * 60 + m) * 60 + s_) * 1000 + f

    def settle(quiet, cap):
        """Wait until the number of presses has not changed for `quiet` seconds (at most `cap`)."""
        end, last, since = time.time() + cap, -1, time.time()
        while time.time() < end:
            n = len(stems())
            if n != last:
                last, since = n, time.time()
            elif time.time() - since >= quiet:
                return
            time.sleep(0.2)
        sys.exit(f"FAIL: screenshots were still arriving after {cap} s")

    def clipboard_paths():
        out = ps("-Command", CLIP).stdout.replace("\r", "").splitlines()
        text = next(l for l in out if l.startswith("text="))[5:]
        return text.split("<NL>")

    def last_stem_paths(stem_list):
        last = sorted(stem_list, key=ms)[-1]
        mine = re.compile(re.escape(last) + r"(-\d+of\d+)?\.png$")
        return [win(os.path.join(TMP, f)) for f in pngs() if mine.match(f)]

    # 1. A burst that fits the queue: every press a file, grabbed when pressed.
    N = 7
    burst(N, 0.04)
    wait_for(len(before) + N, secs=60)
    new = sorted(set(stems()) - before, key=ms)
    assert len(new) == N, f"{N} presses gave {len(new)} files: {new}"
    t = [ms(x) for x in new]
    gaps = [b - a for a, b in zip(t, t[1:])]
    span = t[-1] - t[0]
    # Posted 40 ms apart, the grabs are limited by how long one grab takes, not by saving. With
    # saving inline (measured 0.35-0.4 s a press) every gap is 350 ms or more and the span 2.4 s or
    # more; here the grabs must come far closer than that.
    assert max(gaps) < 250 and span < N * 150, f"{N} presses: gaps {gaps} ms, {span} ms in all: saving is holding up the grabs"
    settle(2.0, 60)  # the worker is still saving the tail of the burst
    got, want = clipboard_paths(), last_stem_paths(new)
    assert got == want, f"the clipboard does not hold the LAST press: has {got}, want {want}"
    print(f"OK burst: {N} presses, {N} files; grabs {span} ms apart in all (gaps {min(gaps)}-{max(gaps)} ms); the clipboard holds the last one")

    # 2. A burst longer than the backlog allows: some presses may be dropped (logged), but nothing
    # hangs, the clipboard still ends on the last SAVED press, and the next single press works.
    before2 = set(stems())
    burst(14, 0.04)
    settle(2.0, 60)  # until no new screenshot has appeared for 2 s
    new2 = sorted(set(stems()) - before2, key=ms)
    assert 7 <= len(new2) <= 14, f"14 quick presses gave {len(new2)} files"
    status = run("hotkey", "--status")[1]
    assert status == "running", f"the hotkey is not running after a long burst (--status said {status!r}; see hotkey.log)"
    got, want = clipboard_paths(), last_stem_paths(new2)
    assert got == want, f"the clipboard does not hold the last saved press: has {got}, want {want}"
    before3 = set(stems())
    press()
    wait_for(len(before3) + 1)
    print(f"OK long burst: 14 presses gave {len(new2)} files (the rest dropped and logged), the hotkey still answers")

    print("== stop during a burst")
    before4 = set(stems())
    burst(6, 0.04)
    rc, out = run("hotkey", "--stop")  # returns only when the daemon is gone
    assert out == "stopped", out
    new4 = sorted(set(stems()) - before4, key=ms)
    assert len(new4) == 6, f"stopping right after 6 presses left {len(new4)} saved: frames already taken were lost"
    got, want = clipboard_paths(), last_stem_paths(new4)
    assert got == want, f"the clipboard does not hold the last press after a stop: has {got}, want {want}"
    print("OK stop: every frame already taken was saved, the clipboard holds the last, and --stop waited for it")

    print("== restart")
    time.sleep(0.5)
    assert run("hotkey", "--status")[1] == "not running"
    rc, out = run("hotkey", "--key", args.key, "--dir", win(TMP), "--no-sound", "--no-flash")
    assert rc == 0, f"restart: {out}"
    print("OK stop and restart")
    print("ALL OK")
    ok = True
except BaseException:
    print(f"(the screenshots of this run are kept for inspection in {TMP})")
    raise
finally:
    run("hotkey", "--stop")
    if ok:  # the files are pictures of the real desktop: do not leave them behind
        shutil.rmtree(TMP, ignore_errors=True)
