//_DEFINES_
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform float alpha;
#if defined(DEBUG_FLAGS)
uniform float tint;
#endif
void main() {
    float a = step(0.001, texture2D(tex, v_coords).a);
    gl_FragColor = vec4(a) * alpha;
}
