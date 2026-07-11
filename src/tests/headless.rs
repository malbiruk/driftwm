use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::utils::{Logical, Point, Size, Transform};

use crate::state::{DriftWm, init_output_state, output_logical_size};

/// Create a fake `HEADLESS-{n}` output the way the real backends do — mode,
/// wl_output global, per-output viewport state, then a Space mapping at the
/// centred camera — minus the renderer, dmabuf global, and render timer a real
/// backend also installs. Outputs tile left-to-right by creation order.
pub fn add_output(state: &mut DriftWm, n: u8, size: (u16, u16)) -> Output {
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
    // Auto layout position: sum of existing outputs' widths, like the udev
    // backend's connection-order placement.
    let layout_position: Point<i32, Logical> = {
        let auto_x: i32 = state
            .space
            .outputs()
            .map(|o| output_logical_size(o).w)
            .sum();
        (auto_x, 0).into()
    };

    let mode = Mode {
        size: Size::from((i32::from(size.0), i32::from(size.1))),
        refresh: 60_000,
    };
    output.change_current_state(
        Some(mode),
        Some(Transform::Normal),
        None,
        Some(layout_position),
    );
    output.set_preferred(mode);
    output.create_global::<DriftWm>(&state.display_handle);

    // Centre the viewport so canvas origin sits in the middle of the output,
    // matching the initial camera both real backends compute.
    let logical = output_logical_size(&output);
    let camera = Point::from((-(logical.w as f64) / 2.0, -(logical.h as f64) / 2.0));

    init_output_state(&output, camera, state.config.drift, layout_position);

    if state.focused_output.is_none() {
        state.focused_output = Some(output.clone());
        // Center the pointer on the first output like the real backends do —
        // focus-follows-mouse scenarios must start from the state a real
        // session starts from.
        let (cam, zoom) = {
            let os = crate::state::output_state(&output);
            (os.camera, os.zoom)
        };
        let center = Point::from((
            cam.x + logical.w as f64 / (2.0 * zoom),
            cam.y + logical.h as f64 / (2.0 * zoom),
        ));
        state.warp_pointer(center);
    }

    state.space.map_output(&output, camera.to_i32_round());
    state.recompute_decoration_scale();

    output
}
