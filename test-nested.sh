#!/usr/bin/env bash
# Test driftwm nested inside current Wayland session (winit backend).
# Usage: ./test-nested.sh [--release]
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

# Build
if [[ "$1" == "--release" ]]; then
    echo "[build] cargo build --release"
    cargo build --release 2>&1
    BINARY="target/release/driftwm"
else
    echo "[build] cargo build"
    cargo build 2>&1
    BINARY="target/debug/driftwm"
fi

echo ""
echo "[run] Starting nested driftwm (winit backend)"
echo "      WAYLAND_DISPLAY is set → winit mode activates automatically"
echo "      Close the window to exit."
echo ""

# Use a separate config so we don't accidentally break the live session.
# Falls back to ~/.config/driftwm/config.toml if test config doesn't exist.
TEST_CONFIG="$SCRIPT_DIR/test-config.toml"
if [[ ! -f "$TEST_CONFIG" ]]; then
    TEST_CONFIG="$HOME/.config/driftwm/config.toml"
    echo "[warn] No test-config.toml found, using live config: $TEST_CONFIG"
fi

echo "[config] $TEST_CONFIG"
echo ""

exec env DRIFTWM_CONFIG="$TEST_CONFIG" "$BINARY" 2>&1 | tee /tmp/driftwm-test.log
