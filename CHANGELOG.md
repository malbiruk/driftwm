# Changelog

## Unreleased

- Multi-GPU (PRIME) support on the udev backend: every usable GPU is opened at
  startup (the system primary renders; outputs on other GPUs scan out via an
  implicit PRIME copy), whole GPUs hot-plug and hot-unplug at runtime (eGPU
  docks), and connector hotplug is routed to the owning device. Also fixes
  direct scanout of client buffers, which the framebuffer exporter previously
  vetoed for all devices.
