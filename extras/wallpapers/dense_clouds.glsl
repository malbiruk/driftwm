// Dense clouds — stormy sky with gold sun backlighting
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
    mat2 rot = mat2(0.8, 0.6, -0.6, 0.8);
    for (int i = 0; i < 6; i++) {
        sum += amp * noise(p);
        p = rot * p * 2.0;
        amp *= 0.5;
    }
    return sum;
}

void main() {
    vec2 uv = (v_coords * size + u_camera) / 380.0;

    // Slow wind drift
    uv.x += u_time * 0.018;
    uv.y += u_time * 0.007;

    // Domain warp for organic billowing shapes
    float warpX = fbm(uv * 0.7 + vec2(0.0, u_time * 0.012));
    float warpY = fbm(uv * 0.8 + vec2(u_time * 0.010, 5.3));
    vec2 warped = uv + vec2(warpX - 0.5, warpY - 0.5) * 3.0;

    float cloud  = fbm(warped);
    float detail = fbm(warped * 2.5 + vec2(4.1, 2.3));
    float micro  = fbm(warped * 5.0 + vec2(1.7, 6.8));

    // Weighted sum of three frequency bands; micro adds fine edge detail
    float density = clamp(
        smoothstep(0.30, 0.68, cloud)  * 0.72 +
        smoothstep(0.38, 0.74, detail) * 0.22 +
        smoothstep(0.46, 0.80, micro)  * 0.10,
        0.0, 1.0
    );

    // Sun backlighting: glow is strongest on dense cloud edges
    float sunLight = fbm(warped * 1.2 + vec2(0.0, 1.5));
    float sunGlow  = smoothstep(0.55, 0.85, sunLight) * density * 0.55;

    vec3 sky        = vec3(0.157, 0.220, 0.392);
    vec3 stormGray  = vec3(0.157, 0.165, 0.235);
    vec3 midCloud   = vec3(0.322, 0.333, 0.455);
    vec3 brightEdge = vec3(0.639, 0.655, 0.745);
    vec3 sunColor   = vec3(0.980, 0.847, 0.600);

    vec3 col = mix(sky, stormGray, density);
    col = mix(col, midCloud,   smoothstep(0.28, 0.62, density));
    col = mix(col, brightEdge, smoothstep(0.58, 0.84, density) * 0.35);
    col = mix(col, sunColor,   sunGlow * 0.50);

    gl_FragColor = vec4(col, 1.0) * alpha;
}
