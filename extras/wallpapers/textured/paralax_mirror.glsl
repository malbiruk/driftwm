// Mirrored Parallax — tiles an image across the canvas with a parallax depth effect
// that responds to camera movement. It uses a mirrored repeat (kaleidoscope) technique
// to eliminate harsh seams, making the texture tile infinitely and smoothly.
//
//     [background]
//     type = "shader"
//     path = "~/.config/driftwm/wallpapers/mirrored_parallax.glsl"
//     texture = "~/Pictures/your_image.png"
//
// `tex` is the configured image. u_texture_size / u_output_size are the image's
// and the viewport's pixel sizes — texture shaders get no built-in `size`, and
// GLSL ES 1.0 has no textureSize(). Zoom is applied externally, so we work in
// canvas space. Backgrounds are opaque, so no `alpha` uniform is needed.

precision highp float; // Set high precision for floating-point calculations to prevent jittering

// Inputs from driftwm
varying vec2 v_coords;         // Normalized screen coordinates from the vertex shader (0.0 to 1.0)
uniform sampler2D tex;         // The background texture image

uniform vec2 u_camera;         // Current camera/viewport position in pixels
uniform vec2 u_output_size;    // Dimensions of the viewport/monitor in pixels
uniform vec2 u_texture_size;   // Dimensions of the source texture in pixels

// Parallax intensity: lower values make the background appear further away
const float PARALLAX_FACTOR = 0.2;

void main() {
    // Apply the parallax factor to the camera movement to create the illusion of depth
    vec2 parallax_camera = u_camera * PARALLAX_FACTOR;

    // Wrap the camera coordinates over a period of TWO texture sizes.
    // This perfectly synchronizes the reset jump with the mirrored tile cycle, preventing motion stutter.
    vec2 wrapped_camera = mod(parallax_camera, u_texture_size * 2.0);

    // Map screen pixel coordinates combined with the camera offset into un-clamped texture space units
    vec2 tile_uv = (v_coords * u_output_size + wrapped_camera) / u_texture_size;

    // Mirrored Repeat algorithm: transforms linear coordinates into a ping-pong wave (0 -> 1 -> 0).
    // This flips the texture back and forth, creating a seamless, kaleidoscope-like connection.
    vec2 mirrored_uv = 1.0 - abs(mod(tile_uv, 2.0) - 1.0);

    // Fetch the RGB color from the texture using the seamless mirrored coordinates
    vec3 col = texture2D(tex, mirrored_uv).rgb;

    // Output the final pixel color with full opacity
    gl_FragColor = vec4(col, 1.0);
}
