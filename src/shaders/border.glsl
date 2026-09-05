// border.glsl — uniform-thickness stroke around a rounded rectangle.
// The stroke sits OUTSIDE the inner rect; u_outer_radius comes from the
// compositor: u_inner_radius + u_border_width for a rounded rect (a shared
// radius would pinch the stroke at corners), or 0 for a square rect so the
// ring stays square instead of rounding by the stroke width.
// u_aa_scale converts shader-logical distances to output pixels, keeping the
// anti-aliasing band one pixel wide at any scale or zoom.
precision highp float;
varying vec2 v_coords;
uniform float alpha;
uniform vec2 size;          // element size in shader-logical units (see u_aa_scale)
uniform vec4 u_inner_rect;  // (x, y, w, h) of inner content rect within element
uniform float u_inner_radius;
uniform float u_border_width;
uniform float u_outer_radius;
uniform float u_aa_scale;   // output pixels per shader-logical unit
uniform vec4 u_color;

float sd_rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + vec2(r);
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 pixel = v_coords * size;
    vec2 inner_center = u_inner_rect.xy + u_inner_rect.zw * 0.5;
    vec2 inner_half = u_inner_rect.zw * 0.5;
    vec2 p = pixel - inner_center;

    vec2 outer_half = inner_half + vec2(u_border_width);

    float sd_inner = sd_rounded_box(p, inner_half, u_inner_radius);
    float sd_outer = sd_rounded_box(p, outer_half, u_outer_radius);

    // Stroke region: outside the inner rect AND inside the outer rect,
    // anti-aliased over a one-output-pixel band at each edge.
    float a_outer = clamp(0.5 - sd_outer * u_aa_scale, 0.0, 1.0);
    float a_inner = clamp(0.5 + sd_inner * u_aa_scale, 0.0, 1.0);
    float coverage = a_outer * a_inner;

    gl_FragColor = u_color * coverage * alpha;
}
