// Dark sea — mostly black with faint foam veins
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

float fbm(vec2 p) {
    float v = 0.0;
    float a = 0.5;
    mat2 rot = mat2(0.8, 0.6, -0.6, 0.8);
    for (int i = 0; i < 5; i++) {
        v += a * noise(p);
        p = rot * p * 2.0;
        a *= 0.5;
    }
    return v;
}

// Ridge noise: sharp bright lines at noise transitions (like foam)
float ridge(vec2 p) {
    float v = 0.0;
    float a = 0.5;
    mat2 rot = mat2(0.8, 0.6, -0.6, 0.8);
    for (int i = 0; i < 5; i++) {
        float n = noise(p);
        // abs(n - 0.5) creates ridges at the 0.5 contour lines
        n = 1.0 - abs(n - 0.5) * 2.0;
        n = n * n; // sharpen the ridges
        v += a * n;
        p = rot * p * 2.0;
        a *= 0.5;
    }
    return v;
}

void main() {
    vec2 canvas = (v_coords * size + u_camera) * 0.008;

    // Gentle warp for organic feel
    float wx = fbm(canvas + vec2(4.1, 2.3));
    float wy = fbm(canvas + vec2(1.7, 8.2));
    vec2 warped = canvas + vec2(wx, wy) * 1.5;

    // Ridge noise for foam/vein patterns
    float r = ridge(warped);

    // Fine detail layer at higher frequency
    float detail = ridge(warped * 3.0 + vec2(3.3, 7.1));

    // Higher thresholds = sparser foam, same brightness
    float foam = smoothstep(0.55, 0.85, r) * 0.5
               + smoothstep(0.6, 0.9, detail) * 0.3;

    // Base color: dark slate with slight blue-green tint
    vec3 dark = vec3(0.10, 0.11, 0.13);
    vec3 mid  = vec3(0.15, 0.17, 0.19);
    vec3 foam_color = vec3(0.55, 0.55, 0.50);

    // Subtle depth variation in the dark areas
    float depth = fbm(canvas * 0.5);
    vec3 base = mix(dark, mid, depth * 0.4);

    vec3 col = mix(base, foam_color, foam);

    gl_FragColor = vec4(col, 1.0) * alpha;
}
