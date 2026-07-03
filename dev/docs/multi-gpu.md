# Multi-GPU (PRIME) architecture

How driftwm drives outputs that hang off different GPUs (issue #91) — e.g. a
hybrid laptop whose internal panel is on the iGPU while the HDMI port is muxed
to the dGPU. Reference implementation: niri's `src/backend/tty.rs`.

## The model: render on one GPU, scan out on any

driftwm never splits rendering across GPUs. Every frame for every output is
composited on the **primary GPU** (the first usable GPU found at startup, its
render node stored as `UdevRenderer::primary_render_node`). What varies per
output is *scanout*:

- Output on the primary GPU: the composited buffer is scanned out directly.
- Output on another GPU: the frame is copied to a buffer allocated on that
  GPU's GBM device, and that copy is scanned out (**implicit PRIME**).

smithay's `GpuManager` hides the copy. `backend/udev.rs` holds a
`GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>` — one GLES renderer
per DRM render node — and the render loop picks per surface:

```text
render_node == primary  →  gpu_manager.single_renderer(&primary)
render_node != primary  →  gpu_manager.renderer(&primary, &target, compositor.format())
```

Both return a `MultiRenderer` (aliased `MultiGpuRenderer`), so the rest of the
frame path is identical; in the cross-GPU case its `finish()` performs the
copy (dma-buf import on the target when the driver allows it, CPU copy as a
fallback).

## Device bookkeeping

- `DriftWm::udev_devices: HashMap<DrmNode, UdevDevice>` — one entry per KMS
  (primary) node. `UdevDevice` is an `Rc<RefCell<DeviceData>>` so the render
  loop, VBlank handlers, and the gamma/DPMS protocol handlers can each hold a
  handle.
- `DeviceData` owns the `DrmDevice`, `GbmDevice`, connector scanner, and the
  per-CRTC `SurfaceData` map. It records both its `kms_node` and its
  `render_node` — distinct on split-DRM systems, where a display-only KMS
  device routes rendering through another GPU's render node. Several KMS
  devices may share one render node; the GPU manager entry is only released
  with the last of them.
- `SurfaceData` (one per active CRTC) owns the `GbmDrmCompositor`, whose
  allocator and framebuffer exporter target *that device's* GBM. The exporter
  is created with the device's render node as import filter — required for
  direct scanout of client buffers (a `NodeFilter::None` silently vetoes it).

All GPUs are opened at startup (`open_gpu` → `attach_gpu`); `UdevEvent::Added`
/ `Removed` add and remove whole devices at runtime, each with its own VBlank
event source. Connector hotplug within a device is a separate, per-device
scan (`scan_device_connectors`).

If the *primary* GPU disappears, there is no promotion of a new primary (niri
behaves the same): surviving outputs go dark until restart. `gpu_removed`
still tears down cleanly — the dmabuf global is disabled and destroyed on a
delay, per-surface feedbacks are cleared so nothing keeps advertising the
dead node, and the render node is dropped from the GPU manager.

## The renderer abstraction (`DriftRenderer`)

The whole compose and capture pipeline is generic over
`R: DriftRenderer` (`render/renderer.rs`), instantiated at
`R = MultiGpuRenderer` by the udev backend and `R = GlesRenderer` by winit.
The trait bounds cover what the pipeline needs (`ImportAll + ImportMem +
ExportMem + Bind<Dmabuf> + Offscreen<GlesTexture> + …`) plus two escape
hatches, `AsGlesRenderer` / `AsGlesFrame`, that expose the underlying
GLES renderer/frame of the *render* GPU.

Elements split into two families:

- **Scanout candidates** (client content: `WaylandSurfaceRenderElement<R>`,
  `MemoryRenderBufferRenderElement<R>`) stay generic over `R`, so the
  `DrmCompositor` on a secondary GPU can still promote them to planes.
- **Gles-only effects** (shader backgrounds, shadows, borders, blur textures,
  tile chunks) only implement `RenderElement<GlesRenderer>`. They are adapted
  with `GlesBridge<E>` (`render/bridge.rs`) or a hand-written
  `RenderElement<MultiGpuRenderer>` impl that draws via `as_gles_frame()`.
  These are never scanned out, only composited.

`drift_render_elements!` generates the `OutputRenderElements<R>` enum with
`RenderElement` impls for both concrete renderers.

### The bridged-damage rule

smithay's `MultiFrame` tracks which regions were drawn through *its own*
methods (`clear`, `draw_solid`, `render_texture_from_to`) and its cross-GPU
`finish()` only copies those regions to the target GPU. Drawing through
`as_gles_frame()` bypasses that tracking — the pixels land on the render GPU
but are never copied, leaving stale trails on the secondary output wherever
only bridged elements repainted (driftwm's background is an opaque
full-screen bridged element, so the damage tracker never `clear()`s under it;
niri dodges this by accident because its damaged pixels are always covered by
a recorded `clear` or a stock smithay element).

Every `RenderElement<MultiGpuRenderer>::draw` impl that goes through
`as_gles_frame()` must therefore first call
`render::bridge::record_bridged_damage(frame, dst, damage)` — a fully
transparent `draw_solid` on the `MultiFrame`: a visual no-op (blending stays
enabled for non-opaque colors) that records the damage for the PRIME copy.

## Per-surface dmabuf feedback

Besides the default dmabuf global (primary render node), each output surface
builds a `SurfaceDmabufFeedback { render, scanout }` (`surface_dmabuf_feedback`
in udev.rs):

- `render` steers allocation to the primary render node — right for anything
  that will be composited.
- `scanout` adds `TrancheFlags::Scanout` tranches targeting the output's KMS
  device, listing plane formats intersected with the primary's render formats
  (so a buffer that fails the scanout test still has a composite path).
  Primary-plane-only formats come first, then primary-or-overlay. For
  cross-GPU outputs only `Linear` modifiers are kept — nominally shared tiled
  modifiers scan out glitched across devices (same workaround as niri).

After each successful frame, `render::send_dmabuf_feedbacks` routes one of the
two to every surface whose primary scanout output is this output, choosing via
smithay's `select_dmabuf_feedback`: surfaces that were promoted to (or tried
for) direct scanout get `scanout`, everyone else gets `render`. smithay dedups
sends by content, so clients only see changes. In practice a client sees the
scanout tranches appear once it goes fullscreen and the compositor first
attempts to put it on a plane — a composited cursor drawn on top blocks that,
so on outputs with `disable_hardware_cursor` the promotion only happens while
the cursor is elsewhere.

To keep the copy off the hot path, `Backend::early_import` runs on every
surface commit (`CompositorHandler::commit`) and starts the buffer import on
the primary GPU immediately, overlapping it with the client's remaining work.

## Debugging notes

- driftwm's tracing goes to **stdout**; capture logs with `> log 2>&1`.
- At output creation, an info log prints the feedback tranche sizes:
  `dmabuf feedback for cardN: X plane formats, Y render-node formats, scanout
  tranches A + B`. `0 + 0` means the plane/render format intersection broke.
- To watch a client receive scanout tranches:
  `WAYLAND_DEBUG=1 mpv --fs … 2>&1 | grep -E "tranche|main_device"` — look
  for `tranche_flags(1)` bursts after going fullscreen.
