#!/bin/sh
# driftwm installer — downloads the latest release and installs system-wide.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/malbiruk/driftwm/main/install.sh | sudo sh
#   curl -fsSL https://raw.githubusercontent.com/malbiruk/driftwm/main/install.sh | sudo sh -s uninstall

set -e

PREFIX="${PREFIX:-/usr/local}"
BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share"
SYSCONFDIR="${SYSCONFDIR:-/etc}"
REPO="malbiruk/driftwm"

# Runtime libraries the binary links against.
RUNTIME_LIBS="libseat.so libdisplay-info.so libinput.so libgbm.so libxkbcommon.so"

red()   { printf '\033[1;31m%s\033[0m\n' "$1"; }
green() { printf '\033[1;32m%s\033[0m\n' "$1"; }
bold()  { printf '\033[1m%s\033[0m\n' "$1"; }

check_root() {
    if [ "$(id -u)" -ne 0 ]; then
        red "Error: must run as root (use sudo)."
        exit 1
    fi
}

detect_distro() {
    if [ -f /etc/os-release ]; then
        . /etc/os-release
        echo "$ID"
    else
        echo "unknown"
    fi
}

check_runtime_deps() {
    missing=""
    for lib in $RUNTIME_LIBS; do
        if ! ldconfig -p 2>/dev/null | grep -q "$lib"; then
            missing="$missing $lib"
        fi
    done

    if [ -n "$missing" ]; then
        red "Missing runtime libraries:$missing"
        echo ""
        distro=$(detect_distro)
        case "$distro" in
            fedora|rhel|centos)
                bold "Install with: sudo dnf install libseat libdisplay-info libinput mesa-libgbm libxkbcommon" ;;
            ubuntu|debian|linuxmint|pop)
                bold "Install with: sudo apt install libseat1 libdisplay-info-dev libinput10 libudev1 libgbm1 libxkbcommon0" ;;
            arch|manjaro|endeavouros)
                bold "Install with: sudo pacman -S seatd libdisplay-info libinput mesa libxkbcommon" ;;
            *)
                bold "Install the packages that provide:$missing" ;;
        esac
        exit 1
    fi
}

do_install() {
    check_root

    bold "Checking runtime dependencies..."
    check_runtime_deps
    green "All runtime dependencies found."

    bold "Fetching latest release..."
    if ! command -v curl >/dev/null 2>&1; then
        red "Error: curl is required."
        exit 1
    fi

    RELEASE_URL=$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"browser_download_url"' \
        | grep 'x86_64-linux\.tar\.gz' \
        | head -1 \
        | sed 's/.*"browser_download_url": *"\([^"]*\)".*/\1/')

    if [ -z "$RELEASE_URL" ]; then
        red "Error: could not find a release artifact."
        red "Check https://github.com/$REPO/releases"
        exit 1
    fi

    TMPDIR=$(mktemp -d)
    trap 'rm -rf "$TMPDIR"' EXIT

    bold "Downloading $RELEASE_URL..."
    curl -fSL "$RELEASE_URL" -o "$TMPDIR/release.tar.gz"
    tar xzf "$TMPDIR/release.tar.gz" -C "$TMPDIR"

    # Find the extracted directory
    SRCDIR=$(find "$TMPDIR" -maxdepth 1 -type d -name 'driftwm-*' | head -1)
    if [ -z "$SRCDIR" ]; then
        red "Error: unexpected archive structure."
        exit 1
    fi

    bold "Installing to $PREFIX..."
    install -Dm755 "$SRCDIR/driftwm" "$BINDIR/driftwm"
    install -Dm755 "$SRCDIR/driftwm-session" "$BINDIR/driftwm-session"
    install -Dm644 "$SRCDIR/driftwm.desktop" "$DATADIR/wayland-sessions/driftwm.desktop"
    install -Dm644 "$SRCDIR/driftwm-portals.conf" "$DATADIR/xdg-desktop-portal/driftwm-portals.conf"

    if [ ! -f "$SYSCONFDIR/driftwm/config.toml" ]; then
        install -Dm644 "$SRCDIR/config.toml" "$SYSCONFDIR/driftwm/config.toml"
    else
        bold "Keeping existing $SYSCONFDIR/driftwm/config.toml"
    fi

    # ── xdg-desktop-portal-wlr screen chooser config ──────────────────────────
    # Install a per-user xdpw config that fixes screen/window capture for OBS,
    # Discord, and other apps that use the XDG ScreenCast portal.
    #
    # Without -f %o slurp returns coordinates; xdpw expects an output name and
    # crashes with "no output found". The -o flag renders output overlays instead
    # of a freeform region selector, which is the correct UX for monitor selection.
    #
    # This writes to the invoking user's config dir (sudo -u $SUDO_USER or $USER).
    # We never overwrite an existing file so user customisations are preserved.
    TARGET_USER="${SUDO_USER:-$USER}"
    TARGET_HOME=$(getent passwd "$TARGET_USER" 2>/dev/null | cut -d: -f6)
    if [ -z "$TARGET_HOME" ]; then
        TARGET_HOME="$HOME"
    fi
    XDPW_DIR="$TARGET_HOME/.config/xdg-desktop-portal-wlr"
    XDPW_CFG="$XDPW_DIR/config"
    if [ ! -f "$XDPW_CFG" ]; then
        mkdir -p "$XDPW_DIR"
        cat > "$XDPW_CFG" <<'EOF'
[screencast]
# slurp -f %o -o returns the output name (e.g. HDMI-A-1) rather than raw
# screen coordinates. xdg-desktop-portal-wlr requires an output name to
# locate the screencopy target; without this flag it crashes on selection.
chooser_type=simple
chooser_cmd=slurp -f %o -o

[screenshot]
chooser_type=simple
chooser_cmd=slurp -f %o -o
EOF
        chown "$TARGET_USER" "$XDPW_CFG" "$XDPW_DIR" 2>/dev/null || true
        bold "Wrote xdg-desktop-portal-wlr config to $XDPW_CFG"
    else
        bold "Keeping existing $XDPW_CFG"
    fi

    for f in "$SRCDIR"/wallpapers/*.glsl; do
        [ -f "$f" ] && install -Dm644 "$f" "$DATADIR/driftwm/wallpapers/$(basename "$f")"
    done

    green "driftwm installed successfully!"
    echo ""
    echo "  Binary:     $BINDIR/driftwm"
    echo "  Session:    $BINDIR/driftwm-session"
    echo "  Config:     $SYSCONFDIR/driftwm/config.toml"
    echo "  Wallpapers: $DATADIR/driftwm/wallpapers/"
    echo "  Portal:     $XDPW_CFG"
    echo ""
    echo "Select 'driftwm' from your display manager, or run 'driftwm' from a TTY."
}

do_uninstall() {
    check_root

    bold "Uninstalling driftwm..."
    rm -f "$BINDIR/driftwm"
    rm -f "$BINDIR/driftwm-session"
    rm -f "$DATADIR/wayland-sessions/driftwm.desktop"
    rm -f "$DATADIR/xdg-desktop-portal/driftwm-portals.conf"
    rm -rf "$DATADIR/driftwm"
    # Don't remove config — user may want to keep it
    green "driftwm uninstalled. Config left at $SYSCONFDIR/driftwm/"
}

case "${1:-install}" in
    install)   do_install ;;
    uninstall) do_uninstall ;;
    *)         red "Usage: $0 [install|uninstall]"; exit 1 ;;
esac
