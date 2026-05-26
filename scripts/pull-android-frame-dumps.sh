#!/usr/bin/env bash
set -euo pipefail

PACKAGE="${1:-com.wzp.desktop}"
OUT_DIR="${2:-android-frame-dumps}"
REMOTE_TAR="/sdcard/wzp-frame-dumps.tar"
LOCAL_TAR="wzp-frame-dumps.tar"
APP_DUMP_DIR="files/com.wzp.desktop/.wzp"

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    echo "Usage: $0 [package] [out-dir]"
    echo "Default package: com.wzp.desktop"
    echo "Default out-dir: android-frame-dumps"
    exit 0
fi

echo ">>> Packaging frame dumps from $PACKAGE..."
adb shell "run-as $PACKAGE tar -C $APP_DUMP_DIR -cf $REMOTE_TAR frame-dumps"

echo ">>> Pulling $REMOTE_TAR..."
adb pull "$REMOTE_TAR" "$LOCAL_TAR"
adb shell "rm -f $REMOTE_TAR" >/dev/null 2>&1 || true

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
tar -xf "$LOCAL_TAR" -C "$OUT_DIR"

echo ">>> Pulled dumps:"
find "$OUT_DIR" -type f | sort | sed 's#^#  #'
