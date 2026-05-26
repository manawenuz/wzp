#!/usr/bin/env bash
set -euo pipefail

PACKAGE="${1:-com.wzp.desktop}"
OUT_DIR="${2:-android-frame-dumps}"
LOCAL_TAR="wzp-frame-dumps.tar"
APP_DUMP_DIR="${WZP_ANDROID_DUMP_ROOT:-.wzp}"
trap 'rm -f "$LOCAL_TAR"' EXIT

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
    echo "Usage: $0 [package] [out-dir]"
    echo "Default package: com.wzp.desktop"
    echo "Default out-dir: android-frame-dumps"
    exit 0
fi

echo ">>> Packaging frame dumps from $PACKAGE..."
adb exec-out "run-as $PACKAGE tar -C $APP_DUMP_DIR -cf - frame-dumps" > "$LOCAL_TAR"

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
tar -xf "$LOCAL_TAR" -C "$OUT_DIR"

echo ">>> Pulled dumps:"
find "$OUT_DIR" -type f | sort | sed 's#^#  #'
