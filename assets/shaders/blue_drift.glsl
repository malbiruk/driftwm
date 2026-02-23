// Procedural marble/fluid background
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

void main() {
    vec2 canvas = (v_coords * size + u_camera) * 0.01;

    float wx = fbm(canvas + vec2(0.0, 0.0));
    float wy = fbm(canvas + vec2(5.2, 1.3));
    float f = fbm(canvas + vec2(wx, wy) * 2.0);

    vec3 col;
    if (f < 0.35) {
        col = mix(vec3(0.05, 0.02, 0.3), vec3(0.1, 0.2, 0.6), f / 0.35);
    } else if (f < 0.65) {
        col = mix(vec3(0.1, 0.2, 0.6), vec3(0.0, 0.7, 0.65), (f - 0.35) / 0.3);
    } else {
        col = mix(vec3(0.0, 0.7, 0.65), vec3(0.5, 0.95, 0.4), (f - 0.65) / 0.35);
    }

    gl_FragColor = vec4(col, 1.0) * alpha;
}
