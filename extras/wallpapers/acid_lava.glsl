// Acid lava — low-saturation ember veins over dark crust
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera;
uniform float u_time;

float hash(vec2 p) {
    return fract(sin(dot(p, vec2(127.1, 311.7))) * 43758.5453);
}

float noise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(hash(i),                  hash(i + vec2(1.0, 0.0)), f.x),
        mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), f.x),
        f.y
    );
}

float fbm(vec2 p) {
    float sum = 0.0, amp = 0.5;
    mat2 rot = mat2(0.80, 0.60, -0.60, 0.80);
    for (int i = 0; i < 5; i++) {
        sum += amp * noise(p);
        p = rot * p * 2.1;
        amp *= 0.5;
    }
    return sum;
}

void main() {
    vec2 uv = (v_coords * size + u_camera) / 260.0;
    vec2 drift = vec2(u_time * 0.07, -u_time * 0.04);

    float base   = fbm(uv * 1.1 + drift);
    float detail = fbm(uv * 2.2 - drift * 1.2 + vec2(base));
    // Higher frequency layer warped by base+detail to form crack-like veins
    float veins  = fbm(uv * 3.0 + vec2(detail, base) * 1.8 - vec2(0.0, u_time * 0.03));

    float molten = smoothstep(0.42, 0.68, base + detail * 0.35);
    // Thin bright lines at the 0.52 contour of the vein noise
    float cracks = 1.0 - smoothstep(0.08, 0.18, abs(veins - 0.52));
    float ember  = molten * (0.60 + cracks * 0.65);

    vec3 crust    = vec3(0.080, 0.050, 0.045);
    vec3 rock     = vec3(0.170, 0.095, 0.070);
    vec3 emberCol = vec3(0.640, 0.260, 0.120);
    vec3 hot      = vec3(0.920, 0.560, 0.220);

    vec3 col = mix(crust, rock, molten * 0.45);
    col += emberCol * ember * 0.65;
    col += hot * pow(ember, 2.2) * 0.30;

    gl_FragColor = vec4(col, 1.0) * alpha;
}
