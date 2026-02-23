// Dot grid background — evenly spaced dots that scroll with the canvas.
// Edit colors, spacing, and dot size directly here.
precision mediump float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;

uniform vec2 u_camera;

// --- Tweak these ---
const vec4 BG_COLOR = vec4(0.173, 0.208, 0.235, 1.0); // #2c353c
const vec4 DOT_COLOR = vec4(0.831, 0.780, 0.667, 1.0); // #d4c7aa
const float DOT_SPACING = 80.0; // pixels between dots at zoom 1.0
const float DOT_RADIUS = 1.0; // dot radius in canvas pixels
// -------------------

void main() {
    // Screen pixel position -> canvas position
    vec2 screen_pixel = v_coords * size;
    vec2 canvas_pos = screen_pixel + u_camera;

    // Distance to nearest grid intersection point
    vec2 grid = mod(canvas_pos, DOT_SPACING);
    vec2 dist = min(grid, DOT_SPACING - grid);
    float d = length(dist);

    float dot_alpha = 1.0 - smoothstep(DOT_RADIUS - 0.5, DOT_RADIUS + 0.5, d);

    gl_FragColor = mix(BG_COLOR, DOT_COLOR, dot_alpha) * alpha;
}
