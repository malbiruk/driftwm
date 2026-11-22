// Per-element surface clipping to a rounded rectangle in geometry-space.
// Wraps every WaylandSurfaceRenderElement of a window (root toplevel and all
// subsurfaces), mapping each element's buffer UV into the window's geometry-
// normalized [0,1] space via `input_to_geo`. Pixels outside geometry are
// discarded; pixels near the rounded corners get alpha-faded.
//
// Combines clipped-surface and rounding-alpha logic into one fragment shader.
//
// With OBSCURE defined (a second compiled variant, used for windows toggled by
// `toggle-obscure`) the sampled color is replaced by the average of its mosaic
// cell, so the picture is unreadable while the clip and rounding below still
// apply unchanged. Cells are laid out in geometry space, which keeps them
// aligned across a window's subsurfaces and scaling with canvas zoom.
//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;

#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

uniform float aa_scale; // output_scale * zoom — keeps AA band ~1 output px wide
uniform vec2 geo_size; // window geometry size (pre-zoom physical)
uniform vec4 corner_radius; // (top_left, top_right, bottom_right, bottom_left)
uniform mat3 input_to_geo; // buffer UV → geometry-normalized [0,1]²

#if defined(OBSCURE)
#define OBSCURE_BLOCK 16.0 // cell edge, logical px

vec4 obscured_sample(vec2 coords_geo) {
    vec2 px = coords_geo * geo_size;
    vec2 cell = floor(px / OBSCURE_BLOCK);
    // Buffer UV moves linearly with geometry space; input_to_geo is a pure
    // scale + translate, so its diagonal is the whole Jacobian.
    vec2 uv_per_geo = 1.0 / vec2(input_to_geo[0][0], input_to_geo[1][1]);
    vec4 sum = vec4(0.0);
    for (int i = 0; i < 3; i++) {
        for (int j = 0; j < 3; j++) {
            vec2 tap_px = (cell + (vec2(float(i), float(j)) + 0.5) / 3.0) * OBSCURE_BLOCK;
            vec2 uv = v_coords + (tap_px / geo_size - coords_geo) * uv_per_geo;
            sum += texture2D(tex, clamp(uv, 0.0, 1.0));
        }
    }
    return sum / 9.0;
}
#endif

float corner_alpha(vec2 coords, vec2 size, vec4 r) {
    vec2 center;
    float radius;
    if (coords.x < r.x && coords.y < r.x) {
        radius = r.x;
        center = vec2(radius, radius);
    } else if (size.x - r.y < coords.x && coords.y < r.y) {
        radius = r.y;
        center = vec2(size.x - radius, radius);
    } else if (size.x - r.z < coords.x && size.y - r.z < coords.y) {
        radius = r.z;
        center = vec2(size.x - radius, size.y - radius);
    } else if (coords.x < r.w && size.y - r.w < coords.y) {
        radius = r.w;
        center = vec2(radius, size.y - radius);
    } else {
        return 1.0;
    }
    float dist = distance(coords, center);
    float t = clamp((dist - radius) * aa_scale + 0.5, 0.0, 1.0);
    return 1.0 - t * t * (3.0 - 2.0 * t);
}

void main() {
    vec3 coords_geo = input_to_geo * vec3(v_coords, 1.0);

#if defined(OBSCURE)
    vec4 color = obscured_sample(coords_geo.xy);
#else
    vec4 color = texture2D(tex, v_coords);
#endif
    #ifdef NO_ALPHA
    color = vec4(color.rgb, 1.0);
    #endif

    if (coords_geo.x < 0.0 || 1.0 < coords_geo.x
            || coords_geo.y < 0.0 || 1.0 < coords_geo.y) {
        color = vec4(0.0);
    } else {
        color = color * corner_alpha(coords_geo.xy * geo_size, geo_size, corner_radius);
    }

    color = color * alpha;

    #if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
    #endif

    gl_FragColor = color;
}
