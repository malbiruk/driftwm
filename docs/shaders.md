# Writing background shaders

driftwm renders the canvas background using a GLSL fragment shader. You can
write your own to replace the default dot grid.

## How it works

The shader runs once per pixel every frame the viewport changes. It receives
the pixel's position and the viewport's camera offset, and outputs a color.
The result covers the entire output behind all windows.

## Inputs

### Built-in (provided by smithay)

| Name       | Type    | Description                                       |
| ---------- | ------- | ------------------------------------------------- |
| `v_coords` | `vec2`  | Normalized position within the output, 0.0–1.0    |
| `size`     | `vec2`  | Output dimensions in pixels (e.g. 1920.0, 1080.0) |

### Custom (provided by driftwm)

| Name       | Type    | Description                                                       |
| ---------- | ------- | ----------------------------------------------------------------- |
| `u_camera` | `vec2`  | Canvas→screen offset in canvas pixels (viewport's top-left)       |
| `u_zoom`   | `float` | Canvas→screen scale (1.0 = unzoomed, >1 zoomed in, <1 zoomed out) |
| `u_time`   | `float` | Seconds since compositor start                                    |

All three are optional — declare only the ones your shader uses. driftwm
detects each at compile time and skips pushing uniforms the shader doesn't
consume, so an unreferenced uniform costs nothing per frame.

`v_coords * size` gives screen-local pixel coordinates (top-left = 0,0).
Adding `u_camera` converts to canvas coordinates — this is how the background
scrolls with the viewport. Without `u_camera`, the shader is fixed to the
screen and doesn't scroll. By default features defined in canvas pixels
grow/shrink with zoom, same as windows; `u_zoom` lets you change that
relationship if you want (e.g. divide a feature's size by `u_zoom` to keep
it screen-sized regardless of zoom level).

## Output

Set `gl_FragColor` to an RGBA `vec4`:

```glsl
gl_FragColor = vec4(color, 1.0);
```

## Examples

### Solid color (cheapest)

No camera, no zoom, no time — uniforms are pushed once at init and never again.
Equivalent in cost to `type = "wallpaper"` with a 1×1 image:

```glsl
precision mediump float;

const vec3 BG = vec3(0.07, 0.07, 0.09);

void main() {
    gl_FragColor = vec4(BG, 1.0);
}
```

### Hue shift across the canvas

Uses `u_camera` so the gradient scrolls with the viewport:

```glsl
precision mediump float;

varying vec2 v_coords;
uniform vec2 size;
uniform vec2 u_camera;

void main() {
    vec2 canvas = (v_coords * size + u_camera) * 0.001;
    vec3 col = vec3(
        sin(canvas.x) * 0.5 + 0.5,
        sin(canvas.y) * 0.5 + 0.5,
        0.5
    );
    gl_FragColor = vec4(col, 1.0);
}
```

## Tips

- **GLSL ES 1.0**: smithay auto-prepends `#version 100`. Don't add your own
  version directive. Use `precision mediump float;` or `highp` for noise.
- **Canvas coords**: The standard pattern is
  `vec2 canvas = (v_coords * size + u_camera) * scale;` where `scale`
  controls the feature size (smaller = larger features).
- **Float precision**: `u_camera` can be large (thousands of pixels from
  origin). If your shader uses `mod()` or `fract()` on canvas coords,
  reduce first: `mod(u_camera, period)` instead of `mod(canvas, period)`.
  See `dot_grid.glsl` for an example. Noise-based shaders using
  `floor()`/`fract()` internally are naturally resilient since the hash
  functions wrap.
- **Animated shaders**: `u_time` gives seconds since compositor start, enabling
  time-driven animations. driftwm re-renders every frame when a shader uses
  `u_time` — declare it in your shader and it will animate continuously.
- **Zoom-aware shaders**: declare `uniform float u_zoom;` to react to viewport
  zoom. Common pattern: divide canvas-pixel sizes by `u_zoom` to keep features
  the same screen size at any zoom level (e.g. `DOT_RADIUS / u_zoom`).
- **Colors as constants**: Define colors, spacing, and other tunables as
  GLSL `const` values at the top of your shader. This keeps everything in
  one file — no config round-trip needed.
- **Shipped examples**: See `extras/wallpapers/` for dot grid, compass grid,
  noise clouds, dark sea, blue drift, and animated squares.

## Sampling an image (textured shaders)

A `type = "shader"` background can sample a single image by adding a `texture`
path. driftwm loads the image and binds it to the shader's `tex` sampler:

```toml
[background]
type = "shader"
path = "~/shaders/scroll_image.glsl"
texture = "~/Pictures/tile.png"
```

Textured shaders are a slightly different contract from the procedural shaders
above — they're compiled as *texture* shaders, so the input set differs:

| Name             | Type        | Provided by | Description                                   |
| ---------------- | ----------- | ----------- | --------------------------------------------- |
| `tex`            | `sampler2D` | smithay     | The configured image. Sample with `texture2D` |
| `v_coords`       | `vec2`      | smithay     | Normalized position within the output, 0.0–1.0 |
| `u_texture_size` | `vec2`      | driftwm     | Image dimensions in pixels                    |
| `u_output_size`  | `vec2`      | driftwm     | Viewport dimensions in pixels (= output / zoom) |
| `u_camera`       | `vec2`      | driftwm     | Canvas→screen offset in canvas pixels         |
| `u_zoom`         | `float`     | driftwm     | Canvas→screen scale                           |
| `u_time`         | `float`     | driftwm     | Seconds since compositor start                |

Notes that differ from procedural shaders:

- **No built-in `size`** — texture shaders don't get smithay's `size`. Use
  `u_output_size` instead (same value: viewport pixels).
- **No `textureSize()`** — GLSL ES 1.0 lacks it, so the image's resolution
  arrives as `u_texture_size`. You need it to turn canvas pixels into texel UVs.
- **`alpha` is optional** — backgrounds are always opaque, so you don't need to
  declare or multiply by an `alpha` uniform.
- **`cache_shader` is ignored** — the shader-bake cache can't sample a runtime
  texture, so textured shaders always render live.

The headline pattern — sample the image at the canvas position so it scrolls
with the viewport:

```glsl
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform vec2 u_camera;
uniform vec2 u_output_size;
uniform vec2 u_texture_size;

void main() {
    vec2 canvas = v_coords * u_output_size + mod(u_camera, u_texture_size);
    vec2 uv = fract(canvas / u_texture_size);  // fract() tiles it infinitely
    gl_FragColor = texture2D(tex, uv);
}
```

For a showcase of what textured mode uniquely enables — a procedural effect
*on* your image, not just sampling it — see
`extras/wallpapers/textured/ripple.glsl`, which animates a watery distortion
over the tiled image.

## Configuring the background

`[background]` accepts a `type` and a `path`. Three types are supported:

```toml
# Procedural GLSL shader — scrolls with the canvas
[background]
type = "shader"
path = "~/shaders/my_bg.glsl"
# Optional: bind an image the shader can sample via `tex`
# (see "Sampling an image" above)
# texture = "~/Pictures/tile.png"

# Image tiled across the canvas (scrolls with the camera)
[background]
type = "tile"
path = "~/Pictures/tile.png"

# Single image fixed to the viewport (does not scroll or zoom).
# Cheapest mode: zero per-frame uniform updates, so blur and overlays
# above stay cached across pans.
[background]
type = "wallpaper"
path = "~/Pictures/wallpaper.png"
```

The `wallpaper` mode stretches the image to fill the output. Pick an image
sized to your monitor for best results.

## Reloading after edits

The config is automatically reloaded when the file changes. The shader is
re-read from disk on every reload, so touch the config to pick up shader
edits:

```bash
touch ~/.config/driftwm/config.toml
```

To bind this to a key, add to your config:

```toml
[keybindings]
"mod+shift+c" = "spawn touch ~/.config/driftwm/config.toml"
```
