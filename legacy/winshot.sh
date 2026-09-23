#!/usr/bin/env bash
# winshot [-w "window title substring"] [outfile]
# Capture the Windows desktop (all monitors) from WSL.
#   winshot                        -> ./winshot-<ts>.png
#   winshot out.png                -> out.png
#   winshot -w CapCut out.png      -> focus the CapCut window first, then capture
set -euo pipefail

WIN=""
while getopts "w:" o; do
  case $o in
    w) WIN="$OPTARG" ;;
    *) echo "usage: winshot [-w window-title] [outfile]" >&2; exit 2 ;;
  esac
done
shift $((OPTIND - 1))

OUT="${1:-$PWD/winshot-$(date +%Y%m%d-%H%M%S).png}"
PS_SCRIPT='C:\Users\plafayette\AppData\Local\Temp\winshot.ps1'
PNG_WIN='C:\Users\plafayette\AppData\Local\Temp\winshot.png'
PNG_WSL="/mnt/c/Users/plafayette/AppData/Local/Temp/winshot.png"

RESULT=$(powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$PS_SCRIPT" \
           -Out "$PNG_WIN" -Window "$WIN" | tr -d '\r')

case "$RESULT" in
  *NOWINDOW*) echo "winshot: no window matching '$WIN' (captured desktop anyway)" >&2 ;;
esac

cp "$PNG_WSL" "$OUT"
echo "$OUT ($(echo "$RESULT" | tail -1))"
