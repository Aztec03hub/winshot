#!/usr/bin/env bash
# Build winshot.exe, install it on the Windows side, and put the WSL shim on PATH.
#   ./install.sh [BIN_DIR]     default BIN_DIR: ~/.local/bin
set -euo pipefail
cd "$(dirname "$0")"
BIN="${1:-$HOME/.local/bin}"
./build.sh
LOCALAPPDATA_WIN=$(cd /mnt/c && cmd.exe /c 'echo %LOCALAPPDATA%' | tr -d '\r')
DEST=$(wslpath -u "$LOCALAPPDATA_WIN")/winshot
BUILT=$(wslpath -u "${WINSHOT_TARGET_DIR:-$LOCALAPPDATA_WIN\\winshot-build\\target}")/release/winshot.exe
# The exe lives on NTFS: launching it from \\wsl.localhost would add startup cost.
mkdir -p "$DEST" "$BIN"
# A running hotkey keeps winshot.exe locked; stop it, copy, and start it again.
HOTKEY_WAS_RUNNING=
hotkey_note() { [[ -z "$HOTKEY_WAS_RUNNING" ]] || echo "the hotkey was stopped to update winshot.exe; start it again with: winshot hotkey (it comes back by itself at logon if --autostart on was set)"; }
trap hotkey_note EXIT # also when a later step fails, so the user is never left with it silently off
if [[ -x "$DEST/winshot.exe" ]] && [[ "$("$DEST/winshot.exe" hotkey --status 2>/dev/null | tr -d '\r')" == running ]]; then
  HOTKEY_WAS_RUNNING=1
  "$DEST/winshot.exe" hotkey --stop >/dev/null # waits until the process is gone and has released the exe
fi
cp "$BUILT" "$DEST/winshot.exe"
if [[ -e "$BIN/winshot" ]] && ! grep -q 'WSL front-end for winshot.exe' "$BIN/winshot"; then
  cp "$BIN/winshot" "$BIN/winshot.v0.bak"
  echo "backed up the previous $BIN/winshot to $BIN/winshot.v0.bak"
fi
sed "s|@WINSHOT_EXE@|$DEST/winshot.exe|" shim/winshot > "$BIN/winshot"
chmod 755 "$BIN/winshot"
# The Claude Code skill that teaches agents to use it (personal skills: every project).
SKILLS="${WINSHOT_SKILLS_DIR:-$HOME/.claude/skills}"
mkdir -p "$SKILLS/winshot"
cp skill/winshot/SKILL.md "$SKILLS/winshot/SKILL.md"
echo "installed the winshot skill to $SKILLS/winshot"
echo "installed $DEST/winshot.exe and $BIN/winshot"
"$BIN/winshot" list displays
