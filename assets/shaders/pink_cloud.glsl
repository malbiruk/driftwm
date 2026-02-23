// Dreamy shoegaze — soft hazy pink wash, Loveless vibes
precision highp float;

varying vec2 v_coords;
uniform vec2 size;
uniform float alpha;
uniform vec2 u_camera;

vec2 hash2(vec2 p) {
    p = vec2(dot(p, vec2(127.1, 311.7)), dot(p, vec2(269.5, 183.3)));
    return fract(sin(p) * 43758.5453);
}

float noise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    f = f * f * (3.0 - 2.0 * f);
    vec2 a = hash2(i);
    vec2 b = hash2(i + vec2(1.0, 0.0));
    vec2 c = hash2(i + vec2(0.0, 1.0));
    vec2 d = hash2(i + vec2(1.0, 1.0));
    return mix(mix(a.x, b.x, f.x), mix(c.x, d.x, f.x), f.y);
}

// Soft FBM — only 3 octaves for blurry, dreamy feel
float fbm(vec2 p) {
    float v = 0.0;
    float a = 0.5;
    mat2 rot = mat2(0.8, 0.6, -0.6, 0.8);
    for (int i = 0; i < 3; i++) {
        v += a * noise(p);
        p = rot * p * 2.0;
        a *= 0.5;
    }
    return v;
}

void main() {
    // Large scale = big soft shapes
    vec2 canvas = (v_coords * size + u_camera) * 0.004;

    // Gentle warp — just enough to feel organic, not chaotic
    float wx = fbm(canvas + vec2(0.0, 0.0));
    float wy = fbm(canvas + vec2(5.2, 1.3));
    vec2 warped = canvas + vec2(wx, wy) * 1.2;

    // Two soft layers for color blending
    float f1 = fbm(warped);
    float f2 = fbm(warped * 0.7 + vec2(3.3, 7.1));

    // Full sunset range: peach/orange → pink → lavender/purple
    vec3 peach      = vec3(0.95, 0.75, 0.65);  // warm orange end
    vec3 salmon     = vec3(0.90, 0.65, 0.68);  // warm pink
    vec3 rose       = vec3(0.85, 0.62, 0.72);  // true pink center
    vec3 lavender   = vec3(0.75, 0.60, 0.80);  // cool purple end
    vec3 mauve      = vec3(0.68, 0.52, 0.75);  // deeper purple

    // Smooth blend — full orange-to-purple sweep
    vec3 col = mix(peach, lavender, f1);
    col = mix(col, rose, f2 * 0.6);
    col = mix(col, salmon, smoothstep(0.45, 0.65, f1) * 0.5);
    col = mix(col, mauve, smoothstep(0.55, 0.75, f2) * 0.35);

    gl_FragColor = vec4(col, 1.0) * alpha;
}
