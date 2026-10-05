//! Screen-sized key badges over the windows in a zoom-to-fit overview.
use super::elements::{OutputRenderElements, PixelSnapRescaleElement};
use crate::state::{DriftWm, output_logical_size, output_state};
use driftwm::{
    canvas::{CanvasPos, canvas_to_screen},
    config::FontWeight,
};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            element::{
                Kind,
                memory::{MemoryRenderBuffer, MemoryRenderBufferRenderElement},
            },
            gles::GlesRenderer,
        },
    },
    output::Output,
    utils::{Physical, Point, Transform},
};

pub(super) fn elements(
    state: &mut DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    if state.survey_output.as_ref() != Some(output) || !state.survey_active() {
        return Vec::new();
    }
    let (camera, zoom) = {
        let os = output_state(output);
        (os.camera, os.zoom)
    };
    let viewport = output_logical_size(output);
    let scale = output.current_scale().fractional_scale();
    let s = scale.ceil().max(1.0) as i32;
    let ready = driftwm::text::fonts_ready();
    let positions: Vec<_> = state
        .survey_hints
        .iter()
        .map(|hint| {
            if !hint.label.starts_with(&state.survey_prefix) {
                return None;
            }
            let w = hint.target.resolve(&state.stage)?;
            let rect = state.visual_frame_rect(&w)?;
            let top = canvas_to_screen(
                CanvasPos(Point::from((rect.x_low, rect.y_low))),
                camera,
                zoom,
            )
            .0;
            let bottom = canvas_to_screen(
                CanvasPos(Point::from((rect.x_high, rect.y_high))),
                camera,
                zoom,
            )
            .0;
            if bottom.x <= 0.0
                || bottom.y <= 0.0
                || top.x >= viewport.w as f64
                || top.y >= viewport.h as f64
            {
                return None;
            }
            let width = 20 + hint.label.len() as i32 * 16;
            let x = ((top.x + bottom.x - width as f64) / 2.0)
                .clamp(0.0, (viewport.w - width).max(0) as f64);
            let y = (top.y + 8.0).clamp(0.0, (viewport.h - 34).max(0) as f64);
            Some((Point::from((x * scale, y * scale)), width))
        })
        .collect();
    let mut elements = Vec::new();
    for (hint, pos) in state.survey_hints.iter_mut().zip(positions) {
        let Some((pos, width)) = pos else {
            continue;
        };
        if hint
            .buffer
            .as_ref()
            .map(|(cached_scale, cached_ready, _)| (*cached_scale, *cached_ready))
            != Some((s, ready))
        {
            hint.buffer = Some((s, ready, badge(&hint.label, width, s)));
        }
        let buffer = &hint.buffer.as_ref().unwrap().2;
        if let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            pos,
            buffer,
            None,
            None,
            None,
            Kind::Unspecified,
        ) {
            elements.push(OutputRenderElements::Decoration(
                PixelSnapRescaleElement::from_element(
                    elem,
                    Point::<i32, Physical>::from((0, 0)),
                    1.0,
                ),
            ));
        }
    }
    elements
}

fn badge(label: &str, width: i32, s: i32) -> MemoryRenderBuffer {
    let (w, h) = (width * s, 34 * s);
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    let radius = 6 * s;
    for y in 0..h {
        for x in 0..w {
            let dx = (radius - x).max(x - (w - 1 - radius)).max(0);
            let dy = (radius - y).max(y - (h - 1 - radius)).max(0);
            if dx * dx + dy * dy <= radius * radius {
                pixels[((y * w + x) * 4) as usize..((y * w + x) * 4 + 4) as usize]
                    .copy_from_slice(&[25, 25, 30, 255]);
            }
        }
    }
    driftwm::text::rasterize_into(
        &mut pixels,
        w,
        h,
        label,
        "monospace",
        20.0 * s as f32,
        FontWeight::Medium,
        [255, 255, 255, 255],
        10 * s,
    );
    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        s,
        Transform::Normal,
        None,
    )
}
