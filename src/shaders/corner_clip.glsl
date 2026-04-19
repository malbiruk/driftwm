//_DEFINES
precision mediump float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform float alpha;
uniform vec2 u_size;
uniform vec4 u_geo;
uniform float u_radius;
uniform float u_scale;        // output_scale * canvas_zoom — keeps AA band ~1 output px wide
uniform float u_clip_top;     // 1.0 = clip top corners, 0.0 = bottom only
uniform float u_clip_shadow;  // 1.0 = clip everything outside geometry

void main() {
    vec4 color = texture2D(tex, v_coords);
    #ifdef NO_ALPHA
    color = vec4(color.rgb, 1.0);
    #endif

    vec2 pixel = v_coords * u_size;
    vec2 geo_pos = pixel - u_geo.xy;
    vec2 geo_size = u_geo.zw;

    bool outside_geo = geo_pos.x < 0.0 || geo_pos.y < 0.0
                    || geo_pos.x >= geo_size.x || geo_pos.y >= geo_size.y;

    // Kill entire CSD shadow when requested
    if (outside_geo && u_clip_shadow > 0.5) {
        gl_FragColor = vec4(0.0);
        return;
    }

    float clip = 1.0;

    // Clip corners of the geometry rect
    bool is_top = geo_pos.y < u_radius;
    bool is_bottom = geo_pos.y > geo_size.y - u_radius;
    bool is_left = geo_pos.x < u_radius;
    bool is_right = geo_pos.x > geo_size.x - u_radius;

    bool in_corner_zone = (is_left || is_right) && (is_top || is_bottom);
    bool top_allowed = u_clip_top > 0.5;

    if (in_corner_zone && (is_bottom || top_allowed)) {
        vec2 corner;
        if (is_left && is_top) {
            corner = vec2(u_radius, u_radius);
        } else if (is_right && is_top) {
            corner = vec2(geo_size.x - u_radius, u_radius);
        } else if (is_right && is_bottom) {
            corner = vec2(geo_size.x - u_radius, geo_size.y - u_radius);
        } else {
            corner = vec2(u_radius, geo_size.y - u_radius);
        }
        float dist = length(geo_pos - corner) - u_radius;
        float t = clamp(dist * u_scale + 0.5, 0.0, 1.0);
        clip = 1.0 - t * t * (3.0 - 2.0 * t);
    }

    gl_FragColor = color * alpha * clip;
}
