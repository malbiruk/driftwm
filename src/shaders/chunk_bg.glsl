//_DEFINES
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform float alpha;

void main() {
    vec3 color = texture2D(tex, v_coords).rgb;
    // `alpha` is declared by convention but not multiplied in: chunked-bg
    // elements claim full `opaque_regions`, so the output must stay opaque or
    // the damage tracker will incorrectly cull what's behind them.
    gl_FragColor = vec4(color, 1.0);
}
