//_DEFINES_
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform vec2 u_halfpixel;
uniform float u_offset;
uniform float alpha;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    vec4 sum = texture2D(tex, v_coords + vec2(-u_halfpixel.x * 2.0, 0.0) * u_offset);
    sum += texture2D(tex, v_coords + vec2(-u_halfpixel.x, u_halfpixel.y) * u_offset) * 2.0;
    sum += texture2D(tex, v_coords + vec2(0.0, u_halfpixel.y * 2.0) * u_offset);
    sum += texture2D(tex, v_coords + vec2(u_halfpixel.x, u_halfpixel.y) * u_offset) * 2.0;
    sum += texture2D(tex, v_coords + vec2(u_halfpixel.x * 2.0, 0.0) * u_offset);
    sum += texture2D(tex, v_coords + vec2(u_halfpixel.x, -u_halfpixel.y) * u_offset) * 2.0;
    sum += texture2D(tex, v_coords + vec2(0.0, -u_halfpixel.y * 2.0) * u_offset);
    sum += texture2D(tex, v_coords + vec2(-u_halfpixel.x, -u_halfpixel.y) * u_offset) * 2.0;
    gl_FragColor = sum / max(sum.a, 0.001);
}
