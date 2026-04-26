// Fast smoke — animated turbulent Catppuccin Mocha plumes
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera;
uniform float u_time;

// Single-component hash: avoids the second dot product since we never need .y
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
    mat2 rot = mat2(0.8, 0.6, -0.6, 0.8);
    for (int i = 0; i < 5; i++) {
        sum += amp * noise(p);
        p = rot * p * 2.1;
        amp *= 0.48;
    }
    return sum;
}

void main() {
    vec2 uv = (v_coords * size + u_camera) / 280.0;
    float t = u_time * 0.32;

    // Upward rise with lateral turbulence drift
    uv.y -= t * 0.85;
    float driftX = fbm(vec2(uv.y * 1.5, t * 1.2)) - 0.5;
    float driftY = fbm(vec2(uv.x * 1.3, t * 1.0)) - 0.5;
    uv.x += driftX * 0.28;
    uv.y += driftY * 0.15;

    float warp1 = fbm(uv * 1.6 + vec2( t * 0.50, -t * 0.42));
    float warp2 = fbm(uv * 2.0 - vec2( t * 0.45,  t * 0.38));

    // Triple-cascade domain warp for chaotic, non-repeating plumes
    vec2 q = vec2(
        fbm(uv + vec2(warp1 * 0.4, warp2 * 0.35)),
        fbm(uv + vec2(5.2, 1.3) + t * 0.15)
    );
    vec2 r = vec2(
        fbm(uv + 5.0 * q + vec2(1.7, 9.2) + t * 0.35),
        fbm(uv + 5.0 * q + vec2(8.3, 2.8) - t * 0.28)
    );
    float smoke = fbm(uv + 5.5 * r + t * 0.18);

    // Catppuccin Mocha grayscale ramp: crust → mantle → surface → overlay → subtext
    vec3 crust   = vec3(0.067, 0.067, 0.106);
    vec3 mantle  = vec3(0.118, 0.118, 0.180);
    vec3 surface = vec3(0.192, 0.196, 0.267);
    vec3 overlay = vec3(0.364, 0.380, 0.502);
    vec3 subtext = vec3(0.561, 0.576, 0.671);

    vec3 col = mix(crust,   mantle,  smoothstep(0.25, 0.45, smoke));
    col = mix(col, surface,  smoothstep(0.40, 0.60, smoke));
    col = mix(col, overlay,  smoothstep(0.56, 0.74, smoke));
    col = mix(col, subtext,  smoothstep(0.68, 0.88, smoke) * 0.55);

    gl_FragColor = vec4(col, 1.0) * alpha;
}
