//_DEFINES
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform float alpha;

uniform vec2 u_camera;
uniform vec2 u_tile_size;
uniform vec2 u_output_size;

void main() {
    vec2 screen_pixel = v_coords * u_output_size;
    vec2 canvas_pos = screen_pixel + mod(u_camera, u_tile_size);
    // +0.5*u_tile_size centers the image on canvas (0,0) — the wrap seam
    // sits at ±u_tile_size/2 rather than at the canvas origin. Matches the
    // chunked path's `image_position = -dims/2` convention.
    vec2 uv = mod(canvas_pos + u_tile_size * 0.5, u_tile_size) / u_tile_size;
    vec4 color = texture2D(tex, uv);
    #ifdef NO_ALPHA
    color = vec4(color.rgb, 1.0);
    #endif
    gl_FragColor = color * alpha;
}
