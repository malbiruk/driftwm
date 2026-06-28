//! Bottom-edge error bar: internal chrome (not a layer-shell client), so it's a
//! pure render element that input passes through.
//!
//! The rasterized buffer is cached per-output keyed by (text, width, scale):
//! rebuilding it every frame would re-damage the strip continuously and keep
//! the compositor from ever going idle while an error is shown.

use std::collections::BTreeMap;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Transform};

use driftwm::config::FontWeight;

use crate::state::ErrorSource;

use super::elements::{OutputRenderElements, PixelSnapRescaleElement};
use super::renderer::DriftRenderer;

const ERROR_BAR_FONT: &str = "monospace";
/// Text size in logical pixels (supersampled by the output scale below).
const ERROR_BAR_FONT_PX: f32 = 11.0;
/// Left/right inset of the text from the bar edges, logical px.
const ERROR_BAR_PAD_X: i32 = 12;
/// Padding above and below the text, logical px (sets the bar height).
const ERROR_BAR_PAD_Y: i32 = 6;
/// Separator between messages from different sources.
const SEPARATOR: &str = "  •  ";

/// Rasterized error bar cached per output, keyed by `(text, width, scale)`.
pub struct ErrorBarCache {
    key: (String, i32, i32),
    buffer: MemoryRenderBuffer,
}

/// Build the error bar element, or an empty vec when there are no errors.
pub fn build_error_bar_elements<R: DriftRenderer>(
    state: &mut crate::state::DriftWm,
    renderer: &mut R,
    output: &Output,
) -> Vec<OutputRenderElements<R>> {
    let name = output.name();
    if state.errors.is_empty() {
        state.render.cached_error_bar.remove(&name);
        return Vec::new();
    }

    let text = join_errors(&state.errors);
    let viewport = crate::state::output_logical_size(output);
    let output_scale = output.current_scale().fractional_scale();
    let s = state.decoration_scale.max(1);
    let bar_height = ERROR_BAR_FONT_PX.ceil() as i32 + 2 * ERROR_BAR_PAD_Y;
    let width = viewport.w.max(1);

    let key = (text, width, s);
    if state.render.cached_error_bar.get(&name).map(|c| &c.key) != Some(&key) {
        let buffer = render_error_bar(&key.0, width, bar_height, s);
        state
            .render
            .cached_error_bar
            .insert(name.clone(), ErrorBarCache { key, buffer });
    }
    let buffer = &state.render.cached_error_bar.get(&name).unwrap().buffer;

    let loc: Point<f64, Physical> = Point::<i32, Logical>::from((0, viewport.h - bar_height))
        .to_f64()
        .to_physical(output_scale);

    let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        loc,
        buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    ) else {
        return Vec::new();
    };

    vec![OutputRenderElements::Decoration(
        PixelSnapRescaleElement::from_element(elem, Point::<i32, Physical>::from((0, 0)), 1.0),
    )]
}

/// Join all error messages onto one line, collapsing internal whitespace
/// (toml parse errors span multiple lines) so the single-line bar stays clean.
fn join_errors(errors: &BTreeMap<ErrorSource, String>) -> String {
    errors
        .values()
        .map(|m| m.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join(SEPARATOR)
}

/// CPU-render the bar: opaque black fill plus tail-ellipsized white text.
/// `scale` supersamples the buffer (buffer scale = `scale`) so text stays
/// crisp on HiDPI; all geometry below is in physical (buffer) pixels.
fn render_error_bar(text: &str, width: i32, bar_height: i32, scale: i32) -> MemoryRenderBuffer {
    let s = scale.max(1);
    let w = (width.max(1) * s).max(1);
    let h = (bar_height.max(1) * s).max(1);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for px in pixels.chunks_exact_mut(4) {
        px[3] = 255;
    }

    let font_px = ERROR_BAR_FONT_PX * s as f32;
    let pad_x = ERROR_BAR_PAD_X * s;
    let available = w - 2 * pad_x;
    if available > 0 {
        let (fitted, _) =
            driftwm::text::fit_text(text, ERROR_BAR_FONT, font_px, FontWeight::Normal, available);
        driftwm::text::rasterize_into(
            &mut pixels,
            w,
            h,
            &fitted,
            ERROR_BAR_FONT,
            font_px,
            FontWeight::Normal,
            [255, 255, 255, 255],
            pad_x,
        );
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        s,
        Transform::Normal,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_errors_collapses_newlines_and_runs() {
        let mut errors = BTreeMap::new();
        errors.insert(
            ErrorSource::Config,
            "config error:\n  expected `=`".to_string(),
        );
        let joined = join_errors(&errors);
        assert_eq!(joined, "config error: expected `=`");
    }

    #[test]
    fn join_errors_joins_multiple_sources_with_separator() {
        let mut errors = BTreeMap::new();
        errors.insert(ErrorSource::Config, "config error".to_string());
        errors.insert(ErrorSource::Background, "background image".to_string());
        let joined = join_errors(&errors);
        assert_eq!(joined, format!("config error{SEPARATOR}background image"));
    }

    #[test]
    fn join_errors_empty_is_empty_string() {
        assert_eq!(join_errors(&BTreeMap::new()), "");
    }
}
