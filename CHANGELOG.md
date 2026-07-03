# Changelog

## Unreleased

- Multi-GPU (PRIME) support on the udev backend (#91): outputs wired to a
  secondary GPU (e.g. an HDMI port muxed to a dGPU on hybrid laptops) now work
  alongside the primary GPU's outputs. Rendering always happens on the primary
  GPU; frames for secondary-GPU outputs are copied across (implicit PRIME).
  GPUs are enumerated at startup and hot-(un)pluggable at runtime. Clients get
  per-surface dmabuf feedback with scanout tranches, so fullscreen apps can
  reach direct scanout on either GPU; client buffers are imported at commit
  time (`early_import`) to keep cross-GPU latency off the render path.
