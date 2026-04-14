// Animated wave shader example
// Available uniforms:
// - u_camera: vec2 - camera position in canvas coordinates
// - u_time: float - time in seconds since compositor start

precision mediump float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera;
uniform float u_time;

void main() {
    vec2 screen_pixel = v_coords * size;
    vec2 canvas_pos = screen_pixel + u_camera;
    vec2 pos = canvas_pos / 100.0;
    
    // Animated waves
    float wave1 = sin(pos.x * 10.0 + u_time * 2.0) * 0.5 + 0.5;
    float wave2 = sin(pos.y * 10.0 + u_time * 1.5) * 0.5 + 0.5;
    
    vec3 color = vec3(wave1 * 0.3, wave2 * 0.5, 0.7);
    gl_FragColor = vec4(color, 1.0) * alpha;
}
