#!/usr/bin/env bash
# Build winshot.exe from WSL with the Windows-native Rust toolchain (MSVC).
# Sources stay in WSL; the target dir lives on NTFS, where linking is fast.
#   ./build.sh            release build
#   ./build.sh test       run the unit tests (on Windows)
set -euo pipefail
cd "$(dirname "$0")"
CARGO="${WINSHOT_CARGO:-/mnt/c/Users/$USER/.cargo/bin/cargo.exe}"
TARGET="${WINSHOT_TARGET_DIR:-C:\\Users\\$USER\\AppData\\Local\\winshot-build\\target}"
"$CARGO" "${1:-build}" --release --manifest-path "$(wslpath -w "$PWD")\\Cargo.toml" --target-dir "$TARGET" "${@:2}" 2>&1 | tr -d '\r'
exit "${PIPESTATUS[0]}"
