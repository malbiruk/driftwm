//! Off-screen capture of canvas regions to PNG, for `driftwm msg screenshot`.
//!
//! A capture is a virtual viewport — a canvas rectangle rendered at a chosen
//! DPI, independent of any output's current zoom. Captures larger than the GPU's
//! max texture size are rendered tile-by-tile and stitched on the CPU, which is
//! what lets a capture exceed what's on screen.

use std::path::Path;

use image::RgbaImage;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Physical, Rectangle, Scale, Size};

use super::capture::render_elements_to_rgba;
use super::capture_background::CaptureBackground;
use super::{OutputRenderElements, compose_capture_elements};

/// Largest single offscreen tile dimension. Every real GPU supports ≥4096;
/// staying at/under it avoids querying `GL_MAX_TEXTURE_SIZE` while still tiling
/// arbitrarily large captures.
const MAX_TILE: i32 = 4096;

/// Hard ceiling per side on the stitched image, bounding CPU memory
/// (16384² RGBA ≈ 1 GiB).
const MAX_DIM: i32 = 16384;

/// Dimensions of a written capture.
pub struct Capture {
    pub width: u32,
    pub height: u32,
}

/// Render `region` (internal canvas coords: top-left, Y-down) at `dpi_scale`
/// pixels per canvas unit, tiling to honor the texture-size limit, and write a
/// PNG to `path`. Windows are drawn with their full chrome (title bar, border,
/// rounded corners, shadow); `include_background` adds the canvas background
/// (off for an isolated `window` capture, which stays transparent). When
/// `isolate` is `Some`, only that window is composed — see
/// [`compose_capture_elements`].
pub fn capture_region_to_png(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    region: Rectangle<i32, Logical>,
    dpi_scale: f64,
    include_background: bool,
    isolate: Option<&smithay::desktop::Window>,
    path: &Path,
) -> Result<Capture, String> {
    if !(dpi_scale.is_finite() && dpi_scale > 0.0) {
        return Err("scale must be a positive number".into());
    }
    if region.size.w <= 0 || region.size.h <= 0 {
        return Err("capture region is empty".into());
    }

    let total_w = (region.size.w as f64 * dpi_scale).round() as i32;
    let total_h = (region.size.h as f64 * dpi_scale).round() as i32;
    if total_w <= 0 || total_h <= 0 {
        return Err("capture resolves to zero pixels".into());
    }
    if total_w > MAX_DIM || total_h > MAX_DIM {
        return Err(format!(
            "capture {total_w}x{total_h}px exceeds the {MAX_DIM}px per-side limit — \
             lower --scale or shrink the region"
        ));
    }

    let mut buf = vec![0u8; (total_w as usize) * (total_h as usize) * 4];
    let dst_stride = total_w as usize * 4;

    // Built once (decodes textures / coarse LODs up front), emitted per tile.
    let capture_bg = CaptureBackground::prepare(state, renderer, region, include_background);

    let mut ty = 0;
    while ty < total_h {
        let th = (total_h - ty).min(MAX_TILE);
        let mut tx = 0;
        while tx < total_w {
            let tw = (total_w - tx).min(MAX_TILE);

            // Virtual camera = region origin + this tile's pixel offset back in
            // canvas units, so the tile's top-left maps to (0,0) of its buffer.
            let tile_camera = smithay::utils::Point::<f64, Logical>::from((
                region.loc.x as f64 + tx as f64 / dpi_scale,
                region.loc.y as f64 + ty as f64 / dpi_scale,
            ));
            let tile_logical = Size::<i32, Logical>::from((
                (tw as f64 / dpi_scale).ceil() as i32,
                (th as f64 / dpi_scale).ceil() as i32,
            ));

            let elements = compose_capture_elements(
                state,
                renderer,
                tile_camera,
                dpi_scale,
                tile_logical,
                &capture_bg,
                isolate,
            );
            let refs: Vec<&OutputRenderElements> = elements.iter().collect();
            let bytes = render_elements_to_rgba(
                renderer,
                Size::<i32, Physical>::from((tw, th)),
                Scale::from(dpi_scale),
                &refs,
            )?;

            let src_stride = tw as usize * 4;
            for row in 0..th as usize {
                let s = row * src_stride;
                let d = (ty as usize + row) * dst_stride + tx as usize * 4;
                if s + src_stride > bytes.len() {
                    break;
                }
                buf[d..d + src_stride].copy_from_slice(&bytes[s..s + src_stride]);
            }

            tx += tw;
        }
        ty += th;
    }

    let img = RgbaImage::from_raw(total_w as u32, total_h as u32, buf)
        .ok_or("internal error: capture buffer size mismatch")?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    img.save_with_format(path, image::ImageFormat::Png)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;

    Ok(Capture {
        width: total_w as u32,
        height: total_h as u32,
    })
}
