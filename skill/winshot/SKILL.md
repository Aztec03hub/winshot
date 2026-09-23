---
name: winshot
description: See the user's Windows desktop from WSL by taking screenshots Claude can actually read. Use this whenever you need to look at anything on the Windows screen - the user asks what is on their screen or monitor, wants you to check, look at or read an app or window (CapCut, Chrome, Teams, Outlook, an installer, an error dialog), asks for a screenshot, or you need to confirm a GUI change or action really happened - even if they never say "screenshot". Also use it when the user wants a full-resolution screenshot file saved for themselves. Not for Linux GUIs, Playwright or browser-automation screenshots, or phones (use adb there).
---

# winshot

`winshot` (on PATH in WSL) captures the Windows desktop at true physical resolution. Its
default output is sized for you: every file fits Claude's image limits, so nothing
downstream shrinks it, and the smallest text on it is kept at the height Claude was
measured to read reliably (9 px for lowercase, 7 for capitals, 6 for digits). A tall or wide capture is split into tiles instead of being crushed.
An ordinary screenshot of the same screen would be shrunk on the way to you and can lose
most of its text, so use winshot rather than any other capture method.

## The recipe

```bash
mkdir -p /tmp/winshot && winshot --window "CapCut" -o /tmp/winshot/capcut-$(date +%H%M%S).png
```

Keep it one `&&`-chained line: some setups have shell hooks that refuse unchained
multi-line commands that write files.

It prints one line per image written, as `<path> (WxH)`. **Read every printed path, in
order**, with the Read tool. A capture can legitimately come back as several files:

- tiles of one image, in reading order: `capcut-...-1of3.png`, `-2of3`, `-3of3`
- one file per monitor for a whole-desktop capture: `...-DISPLAY1.png`, `...-DISPLAY5.png`

Reading only the first file means answering from part of the screen. Write under `/tmp`
(or a scratch dir) with `-o`: without it winshot writes into the current directory, which
is often the user's repository. A timestamped name keeps an old run's tiles from being
mistaken for new ones; trust only the paths this run printed.

## Pick the smallest target that answers the question

Tokens scale with the pixels you capture, and a smaller target also means larger text.
Prefer, in order:

| need | command |
|---|---|
| one app | `--window "title substring"` or `--exe capcut` or `--pid 1234` |
| an exact window | `winshot list windows --filter capcut`, then `--window 0x1411d6` (the hwnd) |
| one monitor | `--display 1`, `--display primary`, `--display DISPLAY5` |
| part of a screen | `--region x,y,w,h` in physical pixels (`winshot list displays` gives positions) |
| everything | no target flag: every monitor, one file each |

To read something tiny (a tray icon badge, a status-bar number), zoom with `--raw --region
x,y,w,h` on a small area (under about 1000x1000 px): it is lossless, and small enough that
nothing downstream shrinks it, so you see the real pixels. This is the one case where
reading a `--raw` capture is right.

`winshot list windows` shows hwnd, pid, exe, monitor and state (min/max) for every visible
window; use it whenever you are not sure which window the user means. When several windows
match, winshot takes the topmost one with real content and says so on stderr (apps like
CapCut keep blank helper windows above the real one).

Window capture works even when the window is covered by others, and minimized windows are
restored without taking focus, captured, and minimized again. It shows only that window,
so menus, tooltips and popups that are separate windows may be missing: capture the
display for those, with `--delay 500` if they need a moment to open.

## Useful flags

- `-v`: stderr gets, per image, the size Claude will see and its exact token cost, plus
  which text set the scale. Use it when budgeting many captures.
- `--delay MS`: wait before capturing (menus opening, animations, a page still loading).
- `--cursor`: include the mouse pointer.
- `--stdout`: PNG bytes on stdout, only when the result is one image.
- `--from saved.png`: apply the same Claude sizing to an existing PNG, for example a
  full-page browser capture that is too tall to read directly.

## When the user wants a file for themselves

Use `--raw`: one lossless PNG exactly as on screen, saved where they asked
(`-o /mnt/c/Users/<user>/Pictures/shot.png` works; the shim translates WSL paths). Add
`--flash` and/or `--shutter` if they want the camera feedback. Do not read a full-screen
`--raw` image to answer questions: Claude Code shrinks anything over 2000 px before you see
it, which can make small text unreadable. Take a default capture for yourself instead (or a
small `--raw --region` zoom, above).

## Messages and what to do

| message | meaning | action |
|---|---|---|
| `no matching window` | nothing matched the selector | `winshot list windows --filter <word>`, pick the hwnd |
| `N windows match; captured "..."` | several matched | check it picked the right one; else use `--window 0xHWND` |
| `captured as an entirely black frame` | DRM-protected video, or a GPU capture failure | say so; try `--display` instead of `--window` |
| `too little readable text to measure` | a picture with little text | fine; it used a safe default scale |
| `DPI virtualisation is active, refusing to capture` | resolution cannot be trusted | do not work around it; report it to the user |
| `cannot all go to --stdout` | the result is several images | use `-o` |
| `command not found: winshot` | not installed | the repo is `~/claude_projects/winshot`; `./install.sh` builds and installs it |

## Things not to do, and why

- **Do not capture with PowerShell / `System.Windows.Forms`.** It is silently DPI-unaware:
  on a 200% display it returns half the resolution and never says so.
- **Do not resize winshot's output yourself.** It is already the largest size Claude will
  accept and the smallest that keeps its text legible; resizing breaks one or the other.
- **Do not capture the whole desktop out of habit.** Several monitors cost several times the
  tokens of the one window you need.
