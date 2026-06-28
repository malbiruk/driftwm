use smithay::backend::renderer::{
    element::{
        Kind, memory::MemoryRenderBufferRenderElement, surface::WaylandSurfaceRenderElement,
    },
    gles::GlesRenderer,
};
use smithay::input::pointer::{CursorImageStatus, CursorImageSurfaceData};
use smithay::utils::IsAlive;
use smithay::utils::{Physical, Point, Scale};
use smithay::wayland::compositor::with_states;

use driftwm::canvas::{CanvasPos, canvas_to_screen};

use super::elements::OutputRenderElements;

/// Build the cursor render element(s) for the current frame.
/// `camera` and `zoom` are from the output being rendered.
/// Returns `OutputRenderElements` — either xcursor memory buffers or client surface elements.
pub fn build_cursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    camera: Point<f64, smithay::utils::Logical>,
    zoom: f64,
    scale: f64,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    if alpha <= 0.0 {
        return vec![];
    }
    // Touch hides the pointer until the next real mouse/trackpad motion. This
    // gate also clears the KMS hardware-cursor plane on udev, since that plane
    // is driven from these same render elements.
    if state.cursor.hidden_by_touch {
        return vec![];
    }
    let pointer = state.seat.get_pointer().unwrap();
    let canvas_pos = pointer.current_location();
    let screen_pos = canvas_to_screen(CanvasPos(canvas_pos), camera, zoom).0;
    let physical_pos: Point<f64, Physical> = screen_pos.to_physical_precise_round(scale);

    let status = state.cursor.cursor_status.clone();
    let mut result = match status {
        CursorImageStatus::Hidden => vec![],
        CursorImageStatus::Surface(ref surface) => {
            if !surface.alive() {
                state.cursor.cursor_status = CursorImageStatus::default_named();
                return build_xcursor_elements(state, renderer, physical_pos, "default", alpha);
            }
            let hotspot = with_states(surface, |states| {
                states
                    .data_map
                    .get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let pos: Point<i32, Physical> = (
                (physical_pos.x - hotspot.x as f64) as i32,
                (physical_pos.y - hotspot.y as f64) as i32,
            )
                .into();
            let elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                    renderer,
                    surface,
                    pos,
                    Scale::from(1.0),
                    alpha,
                    Kind::Cursor,
                );
            elems
                .into_iter()
                .map(|e| OutputRenderElements::CursorSurface(e.into()))
                .collect()
        }
        CursorImageStatus::Named(icon) => {
            build_xcursor_elements(state, renderer, physical_pos, icon.name(), alpha)
        }
    };

    // Drag-and-drop icon. `offset` is the accumulated `wl_surface.attach` delta
    // in the icon's logical pixels — apply it before scale-conversion to physical.
    if let Some(ref icon) = state.dnd_icon
        && icon.surface.alive()
    {
        let pos: Point<i32, Physical> = (
            (physical_pos.x + icon.offset.x as f64 * scale) as i32,
            (physical_pos.y + icon.offset.y as f64 * scale) as i32,
        )
            .into();
        let surface_elems: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
            smithay::backend::renderer::element::surface::render_elements_from_surface_tree(
                renderer,
                &icon.surface,
                pos,
                Scale::from(1.0),
                alpha,
                Kind::Cursor,
            );
        result.extend(
            surface_elems
                .into_iter()
                .map(|e| OutputRenderElements::CursorSurface(e.into())),
        );
    }

    result
}

/// Build xcursor memory buffer elements for a named cursor icon.
fn build_xcursor_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    physical_pos: Point<f64, Physical>,
    name: &'static str,
    alpha: f32,
) -> Vec<OutputRenderElements> {
    let loaded = state.load_xcursor(name).is_some();
    if !loaded && state.load_xcursor("default").is_none() {
        return vec![];
    }
    let key = if loaded { name } else { "default" };
    let cursor_frames = state.cursor.cursor_buffers.get(key).unwrap();

    let frame_idx = if cursor_frames.total_duration_ms == 0 {
        0
    } else {
        let elapsed =
            state.start_time.elapsed().as_millis() as u32 % cursor_frames.total_duration_ms;
        let mut acc = 0u32;
        let mut idx = 0;
        for (i, &(_, _, delay)) in cursor_frames.frames.iter().enumerate() {
            acc += delay;
            if elapsed < acc {
                idx = i;
                break;
            }
        }
        idx
    };

    let (buffer, hotspot, _) = &cursor_frames.frames[frame_idx];
    let hotspot = *hotspot;

    let pos = physical_pos - Point::from((hotspot.x as f64, hotspot.y as f64));
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        pos,
        buffer,
        Some(alpha),
        None,
        None,
        Kind::Cursor,
    ) {
        Ok(elem) => vec![OutputRenderElements::Cursor(elem)],
        Err(_) => vec![],
    }
}
