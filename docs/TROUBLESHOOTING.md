# Troubleshooting Guide

Common issues and solutions for driftwm users.

## Installation Issues

### Build fails with "package not found"

**Symptoms**: `cargo build` fails with errors about missing libraries.

**Solution**: Install native dependencies for your distro:

**Fedora:**
```bash
sudo dnf install libseat-devel libdisplay-info-devel libinput-devel mesa-libgbm-devel libxkbcommon-devel
```

**Ubuntu/Debian:**
```bash
sudo apt install libseat-dev libdisplay-info-dev libinput-dev libudev-dev libgbm-dev libxkbcommon-dev libwayland-dev
```

**Arch:**
```bash
sudo pacman -S libdisplay-info libinput seatd mesa libxkbcommon
```

### Rust version too old

**Symptoms**: Build fails with edition 2024 errors or syntax errors.

**Solution**: driftwm requires Rust 1.85+. Ubuntu 24.04 ships Rust 1.75 which is too old.

Install via rustup instead:
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update
```

## Runtime Issues

### Compositor crashes on startup

**Check logs**: Run with debug logging to see what's failing:
```bash
RUST_LOG=debug driftwm
```

**Common causes**:
- **No seat access**: On real hardware (TTY), you need seat permissions. Make sure you're in the `seat` or `video` group, or use `seatd`.
- **DRM/KMS conflict**: Another compositor or X server is already running. Switch to a free TTY (Ctrl+Alt+F3) and try again.
- **NVIDIA driver issues**: See NVIDIA section below.

### Black screen after login

**Symptoms**: Display manager shows driftwm session, but after login you get a black screen.

**Solution**: Check if driftwm is actually running:
```bash
ps aux | grep driftwm
journalctl --user -u driftwm  # if using systemd session
```

If it's not running, check display manager logs:
```bash
cat ~/.local/share/xorg/Xorg.0.log  # for X-based DMs
journalctl -u gdm  # for GDM
journalctl -u sddm  # for SDDM
```

### Windows don't appear

**Symptoms**: Compositor starts but applications don't show up.

**Debugging**:
1. Check if the app is actually running: `ps aux | grep <app-name>`
2. Check driftwm logs: `RUST_LOG=debug driftwm 2>&1 | grep -i window`
3. Try a known-working app: `foot` or `alacritty`

**Common causes**:
- **XWayland not available**: Some X11 apps need XWayland. Check if `Xwayland` binary is installed.
- **App crashes on startup**: Run the app from terminal to see its error output.
- **Window is off-canvas**: Try `Mod+W` (zoom-to-fit) to see all windows.

## Input Issues

### Trackpad gestures don't work

**Symptoms**: 3-finger/4-finger swipes do nothing.

**Causes**:
1. **Running nested (winit backend)**: Trackpad gestures are intercepted by the parent compositor (GNOME/KDE). Test on real hardware (TTY) or in a VM.
2. **libinput version too old**: Gesture support requires libinput 1.19+. Check version: `libinput --version`
3. **Trackpad not detected**: Check `libinput list-devices` to see if your trackpad is recognized.

**Workaround**: Use mouse equivalents:
- Pan: `Mod + Left-drag`
- Zoom: `Mod + Scroll`
- Navigate: `Mod + Arrow keys`

### Keyboard shortcuts don't work

**Symptoms**: `Mod+Q`, `Mod+Return`, etc. do nothing.

**Debugging**:
1. Check if `Mod` key is correct: default is `Super` (Windows key). Try `Alt` if your keyboard doesn't have Super.
2. Check config: `cat ~/.config/driftwm/config.toml | grep mod_key`
3. Enable debug logging: `RUST_LOG=debug driftwm 2>&1 | grep -i key`

**Solution**: Override mod_key in config:
```toml
mod_key = "alt"  # or "super"
```

### Keyboard layout stuck on US

**Symptoms**: Can't type in your language, always types English.

**Solution**: Set keyboard layout in config:
```toml
[input.keyboard]
layout = "us,ru"  # comma-separated for multiple layouts
options = "grp:win_space_toggle"  # Super+Space to switch
```

Or use `setxkbmap` equivalent for Wayland:
```bash
# This is set in config, not at runtime
```

## Display Issues

### Blurry text / wrong DPI

**Symptoms**: Text looks blurry or too small/large.

**Solution**: Set fractional scale in config:
```toml
[[outputs]]
name = "eDP-1"  # find with: wlr-randr
scale = 1.5  # or 2.0 for HiDPI
```

Or use `wlr-randr` at runtime:
```bash
wlr-randr --output eDP-1 --scale 1.5
```

### Multi-monitor: wrong arrangement

**Symptoms**: Monitors are in wrong order or overlapping.

**Solution**: Configure output positions:
```toml
[[outputs]]
name = "eDP-1"
position = [0, 0]

[[outputs]]
name = "HDMI-A-1"
position = [1920, 0]  # to the right of eDP-1
```

Or use GUI tool: `wdisplays`

### Screen tearing

**Symptoms**: Horizontal lines during scrolling/animation.

**Causes**:
- **VSync disabled**: driftwm uses VBlank by default, but some drivers ignore it.
- **NVIDIA proprietary driver**: See NVIDIA section below.

**Workaround**: Try different DRM backend options (experimental):
```bash
driftwm --backend udev  # default
```

## Application-Specific Issues

### XWayland apps crash or don't start

**Symptoms**: Steam, Wine, JetBrains IDEs fail to launch.

**Solution**:
1. Check if Xwayland is installed: `which Xwayland`
2. Enable XWayland in config (enabled by default):
```toml
xwayland_enabled = true
```
3. Check logs: `RUST_LOG=debug driftwm 2>&1 | grep -i xwayland`

### Electron apps (VS Code, Discord) are blurry

**Symptoms**: Electron apps ignore fractional scaling.

**Solution**: Force Wayland mode for Electron:
```toml
[env]
ELECTRON_OZONE_PLATFORM_HINT = "wayland"
```

Or launch with flags:
```bash
code --enable-features=UseOzonePlatform --ozone-platform=wayland
```

### Firefox: no hardware acceleration

**Symptoms**: Firefox feels sluggish, videos stutter.

**Solution**: Enable Wayland backend:
```toml
[env]
MOZ_ENABLE_WAYLAND = "1"
```

Check if it worked: open `about:support` in Firefox, look for "Window Protocol: wayland".

## Performance Issues

### High CPU usage when idle

**Symptoms**: driftwm uses 5-10% CPU even when nothing is happening.

**Causes**:
- **Animated background shader**: If your custom shader uses `time` uniform, it re-renders every frame.
- **Rogue client**: Some apps request frames continuously.

**Debugging**:
```bash
# Check which clients are active
RUST_LOG=debug driftwm 2>&1 | grep frame_callback
```

**Solution**: Use static background shader (no `time` uniform), or switch to tiled image:
```toml
[background]
tile_path = "~/.config/driftwm/tile.png"
```

### Laggy animations

**Symptoms**: Pan/zoom animations stutter.

**Causes**:
- **Slow GPU**: Integrated graphics may struggle with blur effects.
- **Too many windows**: 50+ windows with blur enabled.

**Solution**: Disable blur or reduce blur strength:
```toml
[effects]
blur_radius = 1  # default: 2
blur_strength = 1.0  # default: 1.1
```

Or disable blur in window rules:
```toml
[[window_rules]]
app_id = "*"
blur = false
```

## NVIDIA-Specific Issues

### Compositor crashes on NVIDIA

**Symptoms**: Works on Intel/AMD, crashes on NVIDIA proprietary driver.

**Known issues**:
- NVIDIA driver has poor Wayland support
- Explicit sync required (NVIDIA 555+)
- Some DRM features unsupported

**Solutions**:
1. **Update driver**: NVIDIA 555+ has better Wayland support
2. **Enable explicit sync** (if available in your driftwm version):
```toml
[backend]
explicit_sync = true  # experimental
```
3. **Use nouveau** (open-source driver): Better Wayland support but slower 3D.

### Black screen on NVIDIA after suspend

**Symptoms**: After suspend/resume, screen stays black.

**Workaround**: Switch to TTY and back:
```bash
Ctrl+Alt+F3  # switch to TTY3
Ctrl+Alt+F2  # switch back
```

Or restart compositor: `Mod+Ctrl+Shift+Q` then re-login.

## Configuration Issues

### Config changes don't apply

**Symptoms**: Edited `~/.config/driftwm/config.toml` but nothing changed.

**Solution**: driftwm has **hot reload** - changes apply automatically within 1 second. If not:
1. Check for syntax errors: `driftwm --check-config`
2. Check logs: `RUST_LOG=info driftwm 2>&1 | grep -i reload`
3. Some settings require restart: `autostart`, `xwayland_enabled`

### Can't find app_id for window rules

**Symptoms**: Want to create window rule but don't know the app_id.

**Solution**: Check state file while app is running:
```bash
cat $XDG_RUNTIME_DIR/driftwm/state | grep app_id
```

Or use `wlr-randr` equivalent for windows (if available).

## Getting Help

If your issue isn't listed here:

1. **Check logs**: `RUST_LOG=debug driftwm 2>&1 | tee driftwm.log`
2. **Search issues**: https://github.com/malbiruk/driftwm/issues
3. **Report bug**: Include:
   - driftwm version: `driftwm --version`
   - OS/distro: `cat /etc/os-release`
   - GPU: `lspci | grep VGA`
   - Logs: attach `driftwm.log`
   - Config: attach `~/.config/driftwm/config.toml` (redact sensitive data)

## Debug Commands

Useful commands for troubleshooting:

```bash
# Check Wayland socket
echo $WAYLAND_DISPLAY
ls -la $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY*

# List connected outputs
wlr-randr

# List input devices
libinput list-devices

# Check seat permissions
loginctl show-session $XDG_SESSION_ID

# Monitor compositor logs
journalctl --user -f | grep driftwm

# Test if Wayland is working
weston-info  # or wayland-info
```
