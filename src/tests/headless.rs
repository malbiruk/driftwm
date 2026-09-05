use smithay::output::{Mode, Output, PhysicalProperties, Scale, Subpixel};
use smithay::reexports::wayland_server::backend::GlobalId;
use smithay::utils::{Size, Transform};

use crate::state::DriftWm;

/// Create a fake `HEADLESS-{n}` output the way the real backends do — mode,
/// wl_output global — then hand it to [`DriftWm::output_connected`] for the
/// backend-independent connect policy (layout position, per-output viewport
/// state, focus/pointer bootstrap, Space mapping). Skips only the renderer,
/// dmabuf global, and render timer a real backend also installs. Outputs tile
/// left-to-right by creation order. Returns the output plus its `GlobalId`, so
/// the fixture can later disable/remove the global on disconnect.
pub fn add_output(state: &mut DriftWm, n: u8, size: (u16, u16)) -> (Output, GlobalId) {
    add_output_with_saved(state, n, size, &std::collections::HashMap::new())
}

/// Like [`add_output`] but hands `output_connected` a per-output camera seed,
/// exercising the fresh-boot camera restore the durable session store drives.
pub fn add_output_with_saved(
    state: &mut DriftWm,
    n: u8,
    size: (u16, u16),
    saved: &std::collections::HashMap<String, (crate::state::CameraSeed, f64)>,
) -> (Output, GlobalId) {
    add_output_full(state, n, size, saved, None)
}

/// Like [`add_output`] but at a fractional output scale.
pub fn add_output_scaled(
    state: &mut DriftWm,
    n: u8,
    size: (u16, u16),
    scale: f64,
) -> (Output, GlobalId) {
    add_output_full(
        state,
        n,
        size,
        &std::collections::HashMap::new(),
        Some(Scale::Fractional(scale)),
    )
}

fn add_output_full(
    state: &mut DriftWm,
    n: u8,
    size: (u16, u16),
    saved: &std::collections::HashMap<String, (crate::state::CameraSeed, f64)>,
    scale: Option<Scale>,
) -> (Output, GlobalId) {
    let output = Output::new(
        format!("HEADLESS-{n}"),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "driftwm".to_string(),
            model: "headless".to_string(),
            serial_number: n.to_string(),
        },
    );

    let mode = Mode {
        size: Size::from((i32::from(size.0), i32::from(size.1))),
        refresh: 60_000,
    };
    output.change_current_state(Some(mode), Some(Transform::Normal), scale, None);
    output.set_preferred(mode);
    let global = output.create_global::<DriftWm>(&state.display_handle);

    state.output_connected(&output, saved);

    (output, global)
}
