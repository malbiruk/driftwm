# Changelog

## Unreleased

- Made the udev render loop correct across multiple DRM devices: pending DPMS
  transitions are routed to the device that owns each output, and the
  output-management head list is aggregated across every device before
  notifying clients (previously each device overwrote the others'). Dirty-
  marking and rendering iterate every device. No behaviour change with a single
  GPU; this is groundwork for driving a second GPU's outputs.
