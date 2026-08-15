# Writing background shaders

driftwm renders the canvas background using a GLSL fragment shader. You can
write your own to replace the default dot grid.

> [!TIP]
> Looking for ready-made shaders, or want to share your own? Browse the [Gallery](https://github.com/malbiruk/driftwm/discussions/143).

## Your first shader

A background shader returns a color for every pixel of the output. The smallest
one paints a flat color:

```glsl
precision mediump float;

const vec3 BG = vec3(0.07, 0.07, 0.09);

void main() {
    gl_FragColor = vec4(BG, 1.0);
}
```

Save it as `~/shaders/my_bg.glsl` and point the config at it:

```toml
[background]
type = "shader"
path = "~/shaders/my_bg.glsl"
```

driftwm watches the config file, so saving it applies the background. The shader
is re-read from disk on every config reload, so after editing the `.glsl` itself,
trigger one:

```bash
touch ~/.config/driftwm/config.toml
```

> [!NOTE]
> Shaders are GLSL ES 1.0 — smithay prepends `#version 100`, so don't add a
> version directive of your own. Open with `precision mediump float;`, or
> `highp` for noise.

## How it works

The shader runs once per pixel every frame the viewport changes. It receives
the pixel's position and the viewport's camera offset, and outputs a color.
The result covers the entire output behind all windows.

## Inputs

### Built-in (provided by smithay)

| Name       | Type   | Description                                       |
| ---------- | ------ | ------------------------------------------------- |
| `v_coords` | `vec2` | Normalized position within the output, 0.0–1.0    |
| `size`     | `vec2` | Output dimensions in pixels (e.g. 1920.0, 1080.0) |

### Custom (provided by driftwm)

| Name       | Type    | Description                                                       |
| ---------- | ------- | ----------------------------------------------------------------- |
| `u_camera` | `vec2`  | Canvas→screen offset in canvas pixels (viewport's top-left)       |
| `u_zoom`   | `float` | Canvas→screen scale (1.0 = unzoomed, >1 zoomed in, <1 zoomed out) |
| `u_time`   | `float` | Seconds since compositor start                                    |

All three are optional — declare only the ones your shader uses.

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

The alpha component (the `1.0` above) is ignored by default — backgrounds are
composited opaque. To make a shader output its own transparency, set
`transparent_shader = true` (see [Transparent backgrounds](#transparent-backgrounds)).

## Example: hue shift across the canvas

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

- **Canvas coords**: The standard pattern is
  `vec2 canvas = (v_coords * size + u_camera) * scale;` where `scale`
  controls the feature size (smaller = larger features).
- **Float precision**: `u_camera` can be large (thousands of pixels from
  origin). If your shader uses `mod()` or `fract()` on canvas coords,
  reduce first: `mod(u_camera, period)` instead of `mod(canvas, period)`.
  See `extras/wallpapers/dot_grid.glsl` for an example. Noise-based shaders
  using `floor()`/`fract()` internally are naturally resilient since the hash
  functions wrap.
- **Animated shaders**: `u_time` gives seconds since compositor start, enabling
  time-driven animations. driftwm re-renders every frame when a shader uses
  `u_time`, unless `animate_fps` caps the rate.
- **Zoom-aware shaders**: declare `uniform float u_zoom;` to react to viewport
  zoom. Common pattern: divide canvas-pixel sizes by `u_zoom` to keep features
  the same screen size at any zoom level (e.g. `DOT_RADIUS / u_zoom`).
- **Colors as constants**: Define colors, spacing, and other tunables as
  GLSL `const` values at the top of your shader. This keeps everything in
  one file — no config round-trip needed.
- **Shipped examples**: `extras/wallpapers/` holds `dot_grid.glsl` alongside
  `static/` (`blue_drift`, `compass_grid`, `dark_sea`, `pink_cloud`),
  `animated/` (`acid_lava`, `dense_clouds`, `fast_smoke`), and `textured/`
  (`mirrored_parallax`, `ripple`). `make install` copies them to
  `$(PREFIX)/share/driftwm/wallpapers/` — `/usr/local/share/driftwm/wallpapers/`
  by default, `/usr/share/driftwm/wallpapers/` from a distro package.

## Sampling an image (textured shaders)

A `type = "shader"` background can sample a single image by adding a `texture`
path. driftwm loads the image and binds it to the shader's `tex` sampler:

```toml
[background]
type = "shader"
path = "~/shaders/scroll_image.glsl"
texture = "~/Pictures/tile.png"
```

Adding a `texture` compiles the shader as a _texture_ shader, whose input set is:

| Name             | Type        | Provided by | Description                                     |
| ---------------- | ----------- | ----------- | ----------------------------------------------- |
| `tex`            | `sampler2D` | smithay     | The configured image. Sample with `texture2D`   |
| `v_coords`       | `vec2`      | smithay     | Normalized position within the output, 0.0–1.0  |
| `u_texture_size` | `vec2`      | driftwm     | Image dimensions in pixels                      |
| `u_output_size`  | `vec2`      | driftwm     | Viewport dimensions in pixels (= output / zoom) |
| `u_camera`       | `vec2`      | driftwm     | Canvas→screen offset in canvas pixels           |
| `u_zoom`         | `float`     | driftwm     | Canvas→screen scale                             |
| `u_time`         | `float`     | driftwm     | Seconds since compositor start                  |

Notes on the texture path:

| Name            | Note                                                                                                                      |
| --------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `size`          | Not provided — use `u_output_size`, which carries the same value (viewport pixels).                                       |
| `textureSize()` | Not in GLSL ES 1.0 — the image's resolution arrives as `u_texture_size`. You need it to turn canvas pixels into texel UVs. |
| `alpha`         | Always `1.0` for backgrounds. Transparency comes from the shader's own output alpha plus `transparent_shader = true`.      |
| `cache_shader`  | No effect — the shader-bake cache can't sample a runtime texture, so textured shaders always render live.                  |

Tile the image at the canvas position so it scrolls with the viewport:

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

See `extras/wallpapers/textured/ripple.glsl`, which animates a watery
distortion over the tiled image.

## Configuring the background

`[background]` accepts a `type` and, for the source-bearing types, a `path`.
Five types are supported:

```toml
# Built-in dot grid — the default when [background] is absent (no path).
[background]
type = "default"

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
# Cheapest mode: zero per-frame uniform updates.
[background]
type = "wallpaper"
path = "~/Pictures/wallpaper.png"

# No built-in background (no path).
[background]
type = "none"
```

The `wallpaper` mode scales the image to cover the output while preserving its
aspect ratio, centering and cropping any overflow.

Other `[background]` keys, described in full in the
[config reference](config.md#background):

| Key                  | Effect                                                                                                         |
| -------------------- | -------------------------------------------------------------------------------------------------------------- |
| `mirror_tile`        | `tile` mode: mirror-fold the image so a non-seamless edge always meets a reflection.                            |
| `cache_shader`       | Bake a static camera-only shader to textures and pan those. No effect with `transparent_shader` or a `texture`. |
| `transparent_shader` | Honor a shader's output alpha (see below).                                                                     |
| `cache_budget_mb`    | Memory ceiling (MB) for the bake and gigapixel-TIFF chunk caches. Default 128.                                  |
| `animate_fps`        | Frame-rate cap for `u_time` shaders. Default 0 = every output frame.                                            |

### When `cache_shader` is safe

Baking renders the shader once into a texture and pans that texture, so a heavy
static shader ends up costing about what an image costs. It is only correct for
a shader that slides rigidly with the camera — `u_camera` used once, at full
scale, as the only camera term:

```glsl
vec2 canvas = v_coords * size + u_camera;   // pan shifts the image 1:1
```

Parallax (`u_camera * factor`) bakes wrong: the texture pans 1:1 no matter what
factor the shader applied. Shaders reading `u_time` or `u_zoom` are never baked
and always render live, so the flag costs nothing there.

## Transparent backgrounds

By default driftwm composites the background as fully opaque, a fast path that
skips blending and skips redrawing anything beneath it. The background sits
_above_ any `wlr-layer-shell` **Background**-layer surface, so making it
see-through lets an external wallpaper engine (a QuickShell or `swaybg` setup,
say) show through while the built-in background stays on top — to drop the
built-in background entirely instead, use `type = "none"` (below).

Two ways to opt in, depending on background type:

**Images (`tile` / `wallpaper`)** — automatic. If the PNG carries an alpha
channel with any transparent pixels, driftwm honors it: transparent areas blend
to whatever's below. A fully opaque image keeps the fast path. No config needed.

```toml
# Dots-with-transparent-gaps PNG tiled as a spatial reference over a live
# wallpaper engine running on the Background layer — gaps show the engine.
[background]
type = "tile"
path = "~/Pictures/dots.png"
```

**Shaders (`type = "shader"`)** — not autodetected; opt in with
`transparent_shader = true` to honor the shader's output alpha:

```toml
[background]
type = "shader"
path = "~/shaders/dot_grid.glsl"
transparent_shader = true
```

Then output a low (or zero) alpha where you want the layer below to show:

```glsl
// Opaque dots over a transparent field — the gaps reveal what's underneath.
const vec4 BG_COLOR  = vec4(0.0, 0.0, 0.0, 0.0);  // transparent
const vec4 DOT_COLOR = vec4(1.0, 1.0, 1.0, 1.0);  // opaque
```

Notes:

- **Premultiplied alpha** — compositing is premultiplied, so output
  `vec4(rgb * a, a)`. Mixing two valid premultiplied colors (as `dot_grid` does)
  stays valid; a raw `vec4(rgb, 0.5)` would fringe too bright.
- **Cost** — transparency costs a blend every frame plus a repaint of whatever
  sits below, so turn it on only when something is actually behind.

## External wallpaper engines (`type = "none"`)

`type = "none"` renders no built-in background at all, so whatever sits on the
`wlr-layer-shell` **Background** layer becomes the wallpaper — letting you use a
standard Wayland wallpaper daemon instead of driftwm's shader/image modes:

- `swaybg` — static images
- `swww` / `wpaperd` — animated wallpapers and transitions
- `mpvpaper` — **live video** wallpapers (mpv on a layer surface)

Launch the daemon yourself (e.g. from `autostart`); driftwm just gets out of the
way. With nothing on the Background layer, you'll see the clear color (black).

Notes:

- **`path` is ignored** for this type.
- A live video wallpaper damages the whole screen every frame, so it repaints
  continuously (the same cost profile as an animated shader).

## Reloading after edits

driftwm reloads the config automatically when the file changes, and re-reads the
shader from disk on every reload. Bind the reload action to pick up shader edits
without editing the config:

```toml
[keybindings]
"mod+shift+c" = "reload-config"
```

Without a keybinding, touching the config file has the same effect:

```bash
touch ~/.config/driftwm/config.toml
```

If the shader can't be read or fails to compile, driftwm falls back to the
built-in dot grid and reports the reason on the error bar
(`background shader: compile error: …`). A dot grid after an edit means the
shader was rejected, not that the config was ignored. The error clears on the
next reload that succeeds.
