use smithay::{
    backend::input::{
        AbsolutePositionEvent, ButtonState, Device, DeviceCapability, Event, InputBackend,
        ProximityState, TabletToolButtonEvent, TabletToolEvent, TabletToolProximityEvent,
        TabletToolTipEvent, TabletToolTipState,
    },
    input::pointer::MotionEvent,
    utils::SERIAL_COUNTER,
    wayland::tablet_manager::{TabletDescriptor, TabletSeatTrait},
};

use crate::state::DriftWm;
use driftwm::canvas::{ScreenPos, screen_to_canvas};

impl DriftWm {
    pub fn on_device_added<I: InputBackend>(&mut self, device: &I::Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.seat.tablet_seat();
            let desc = TabletDescriptor::from(device);
            tablet_seat.add_tablet::<Self>(&self.display_handle, &desc);
        }
    }

    pub fn on_device_removed<I: InputBackend>(&mut self, device: &I::Device) {
        if device.has_capability(DeviceCapability::TabletTool) {
            let tablet_seat = self.seat.tablet_seat();
            let desc = TabletDescriptor::from(device);
            tablet_seat.remove_tablet(&desc);
            if tablet_seat.count_tablets() == 0 {
                tablet_seat.clear_tools();
            }
        }
    }

    pub fn on_tablet_tool_axis<I: InputBackend>(&mut self, event: I::TabletToolAxisEvent) {
        let output = match self.active_output() {
            Some(o) => o,
            None => return,
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        // Absolute coordinate from 0.0 to 1.0 mapped to output size
        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera(), self.zoom()).0;

        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();

        // Update pointer in seat so normal menus/hover work for legacy applications
        let pointer = self.seat.get_pointer().unwrap();
        let old_focus = pointer.current_focus();
        let under = self.pointer_focus_under(screen_pos, canvas_pos);

        pointer.motion(
            self,
            under.clone(),
            &MotionEvent {
                location: canvas_pos,
                serial,
                time,
            },
        );
        pointer.frame(self);
        self.update_decoration_cursor(canvas_pos);
        self.update_pointer_constraint(old_focus);
        self.maybe_hover_focus(canvas_pos);
        self.refresh_cursor_edge_pan();

        // Forward native tablet events to supporting clients
        let tablet_seat = self.seat.tablet_seat();
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));
        let tool = tablet_seat.get_tool(&event.tool());

        if let (Some(tablet), Some(tool)) = (tablet, tool) {
            if event.pressure_has_changed() {
                tool.pressure(event.pressure());
            }
            if event.distance_has_changed() {
                tool.distance(event.distance());
            }
            if event.tilt_has_changed() {
                tool.tilt(event.tilt());
            }
            if event.slider_has_changed() {
                tool.slider_position(event.slider_position());
            }
            if event.rotation_has_changed() {
                tool.rotation(event.rotation());
            }
            if event.wheel_has_changed() {
                tool.wheel(event.wheel_delta(), event.wheel_delta_discrete());
            }

            let wl_surface_and_pos = under.as_ref().map(|(focus_target, relative_pos)| {
                (focus_target.0.clone(), *relative_pos)
            });

            tool.motion(
                canvas_pos,
                wl_surface_and_pos,
                &tablet,
                serial,
                time,
            );
        }
    }

    pub fn on_tablet_tool_proximity<I: InputBackend>(&mut self, event: I::TabletToolProximityEvent) {
        let output = match self.active_output() {
            Some(o) => o,
            None => return,
        };
        let Some(output_geo) = self.space.output_geometry(&output) else {
            return;
        };

        let screen_pos = event.position_transformed(output_geo.size);
        let canvas_pos = screen_to_canvas(ScreenPos(screen_pos), self.camera(), self.zoom()).0;

        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();

        let under = self.pointer_focus_under(screen_pos, canvas_pos);

        let tablet_seat = self.seat.tablet_seat();
        let display_handle = self.display_handle.clone();
        let tool = tablet_seat.add_tool::<Self>(self, &display_handle, &event.tool());
        let tablet = tablet_seat.get_tablet(&TabletDescriptor::from(&event.device()));

        if let Some(tablet) = tablet {
            match event.state() {
                ProximityState::In => {
                    if let Some((focus_target, relative_pos)) = under {
                        tool.proximity_in(
                            canvas_pos,
                            (focus_target.0, relative_pos),
                            &tablet,
                            serial,
                            time,
                        );
                    }
                }
                ProximityState::Out => {
                    tool.proximity_out(time);
                }
            }
        }
    }

    pub fn on_tablet_tool_tip<I: InputBackend>(&mut self, event: I::TabletToolTipEvent) {
        let tablet_seat = self.seat.tablet_seat();
        let tool = tablet_seat.get_tool(&event.tool());

        let Some(tool) = tool else {
            return;
        };

        let serial = SERIAL_COUNTER.next_serial();
        let time = event.time_msec();

        match event.tip_state() {
            TabletToolTipState::Down => {
                tool.tip_down(serial, time);

                // Emulate pointer button press for drawing and clicking in normal apps
                let pointer = self.seat.get_pointer().unwrap();
                pointer.button(
                    self,
                    &smithay::input::pointer::ButtonEvent {
                        button: 0x110, // BTN_LEFT
                        state: ButtonState::Pressed,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
            }
            TabletToolTipState::Up => {
                tool.tip_up(time);

                // Emulate pointer button release
                let pointer = self.seat.get_pointer().unwrap();
                pointer.button(
                    self,
                    &smithay::input::pointer::ButtonEvent {
                        button: 0x110, // BTN_LEFT
                        state: ButtonState::Released,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
            }
        }
    }

    pub fn on_tablet_tool_button<I: InputBackend>(&mut self, event: I::TabletToolButtonEvent) {
        let tablet_seat = self.seat.tablet_seat();
        let tool = tablet_seat.get_tool(&event.tool());

        if let Some(tool) = tool {
            tool.button(
                event.button(),
                event.button_state(),
                SERIAL_COUNTER.next_serial(),
                event.time_msec(),
            );
        }
    }
}
