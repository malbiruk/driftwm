# 1. Spawner / DISPLAY plumbing

cargo run -- --backend udev # check log for "spawned xwayland-satellite pid=N on :N"
echo $DISPLAY # in driftwm terminal — should print :N
xeyes # connects to the already-running satellite

# 2. Fail-soft when satellite missing

mv ~/.cargo/bin/xwayland-satellite ~/.cargo/bin/xwayland-satellite.bak
cargo run # WARN log, no DISPLAY exported, no panic
mv ~/.cargo/bin/xwayland-satellite.bak ~/.cargo/bin/xwayland-satellite

# 3. Known limitation: no respawn on crash

xeyes &
killall xwayland-satellite
xclock # FAILS — satellite is not respawned, X11 stays dead until driftwm restart

# 4. Real-world apps that exercised the old bugs

flatpak run com.valvesoftware.Steam # original #46 target
xterm # ctrl+rmb menu (popup placement)
gimp # right-click menu trees
java -jar idea.jar # JetBrains menus

# 5. Clipboard bridge (handled inside satellite, no compositor work)

echo "from x11" | xclip -i -selection clipboard
wl-paste # should print "from x11"
echo "from wayland" | wl-copy
xclip -o -selection clipboard # should print "from wayland"
