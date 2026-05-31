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
    // floor(u_tile_size*0.5) centers the image on canvas (0,0) with the wrap
    // seam at ±dims/2. `floor` (not a bare 0.5×) so odd dimensions land on the
    // same integer offset as the chunked path's `image_position = -(dims/2)`,
    // whose i32 division truncates — keeping the shader fallback plane pixel-
    // aligned with the per-tile chunks drawn over it.
    vec2 uv = mod(canvas_pos + floor(u_tile_size * 0.5), u_tile_size) / u_tile_size;
    vec4 color = texture2D(tex, uv);
    #ifdef NO_ALPHA
    color = vec4(color.rgb, 1.0);
    #endif
    gl_FragColor = color * alpha;
}
