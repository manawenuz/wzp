#!/usr/bin/env bash
set -euo pipefail
# Clean up any wzp-builder servers left running
echo "Looking for wzp-builder servers..."
hcloud server list -o noheader | grep wzp-builder | while read -r line; do
  id=$(echo "$line" | awk '{print $1}')
  name=$(echo "$line" | awk '{print $2}')
  echo "  Deleting $name (id=$id)..."
  hcloud server delete "$id"
done
echo "Done."
