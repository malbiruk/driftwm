# Changelog

## Unreleased

### Multi-GPU (PRIME) groundwork

- Render elements are now generic over the renderer (`OutputRenderElements<R>`,
  defaulting to `GlesRenderer`), so the same pipeline works with the plain
  `GlesRenderer` and the cross-GPU `MultiGpuRenderer`. Gles-only effects
  (background/shadow/border/blur) bridge through `GlesBridge`; window, layer, and
  cursor content stays generic so it can still be promoted to scanout planes.
- The live compose path (`compose_frame` and its helpers — layers, cursor,
  shadows/borders, blur, error bar, output outlines) is now generic over
  `R: DriftRenderer`. Offscreen blur work runs on the primary GPU via
  `as_gles_renderer()`. No behaviour change on the existing single-GPU / winit
  path.
