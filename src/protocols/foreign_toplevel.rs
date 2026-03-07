//! `zwlr-foreign-toplevel-management-v1` protocol implementation.
//!
//! Allows external apps (taskbars, docks) to list, activate, and close windows.
//! Adapted from niri's implementation.

use std::collections::hash_map::Entry;
use std::collections::HashMap;

use smithay::desktop::{Space, Window};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::wayland::seat::WaylandFocus;
use smithay::reexports::wayland_protocols_wlr;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1, zwlr_foreign_toplevel_manager_v1,
};
use zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1;
use zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1;

const VERSION: u32 = 3;

pub struct ForeignToplevelManagerState {
    display: DisplayHandle,
    instances: Vec<ZwlrForeignToplevelManagerV1>,
    toplevels: HashMap<WlSurface, ToplevelData>,
}

pub trait ForeignToplevelHandler {
    fn foreign_toplevel_manager_state(&mut self) -> &mut ForeignToplevelManagerState;
    fn foreign_toplevel_outputs(&self) -> Vec<Output>;
    fn activate(&mut self, wl_surface: WlSurface);
    fn close(&mut self, wl_surface: WlSurface);
    fn set_fullscreen(&mut self, wl_surface: WlSurface, wl_output: Option<WlOutput>);
    fn unset_fullscreen(&mut self, wl_surface: WlSurface);
}

struct ToplevelData {
    title: Option<String>,
    app_id: Option<String>,
    states: Vec<u32>,
    /// All WlOutputs we've sent output_enter for, per handle instance.
    instances: HashMap<ZwlrForeignToplevelHandleV1, Vec<WlOutput>>,
}

pub struct ForeignToplevelGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

impl ForeignToplevelManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
        D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ForeignToplevelGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrForeignToplevelManagerV1, _>(VERSION, global_data);
        Self {
            display: display.clone(),
            instances: Vec::new(),
            toplevels: HashMap::new(),
        }
    }
}

/// Sync foreign-toplevel state with the current window list.
/// Call once per frame (not per-output) — windows on the infinite canvas
/// appear on all outputs, so there's no per-output tracking.
///
/// Takes split references to avoid borrow conflicts with DriftWm.
/// Generic over D (the compositor state type) for `create_resource` dispatch.
pub fn refresh<D>(
    ft_state: &mut ForeignToplevelManagerState,
    space: &Space<Window>,
    focused_surface: Option<&WlSurface>,
    outputs: &[Output],
) where
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()> + 'static,
{
    // 1. Remove closed or widget windows
    ft_state.toplevels.retain(|surface, data| {
        let alive = space.elements().any(|w| {
            w.wl_surface().as_deref() == Some(surface)
                && !crate::config::applied_rule(surface).is_some_and(|r| r.widget)
        });
        if !alive {
            for instance in data.instances.keys() {
                instance.closed();
            }
        }
        alive
    });

    // 2. Refresh non-focused windows first (deactivate-before-activate ordering)
    let mut focused_entry = None;
    for window in space.elements() {
        let Some(wl_surface) = window.wl_surface() else { continue; };
        if crate::config::applied_rule(&wl_surface).is_some_and(|r| r.widget) {
            continue;
        }
        let wl_surface = wl_surface.into_owned();
        let is_focused = focused_surface.is_some_and(|fs| fs == &wl_surface);
        if is_focused {
            focused_entry = Some(window.clone());
            continue;
        }
        refresh_toplevel::<D>(ft_state, &wl_surface, outputs, false);
    }

    // 3. Refresh focused window last (with Activated state)
    if let Some(window) = focused_entry {
        let Some(wl_surface) = window.wl_surface().map(|s| s.into_owned()) else { return; };
        refresh_toplevel::<D>(ft_state, &wl_surface, outputs, true);
    }
}

/// Send output_enter for a newly connected output to all existing toplevels.
pub fn send_output_enter_all(
    ft_state: &mut ForeignToplevelManagerState,
    output: &Output,
) {
    for data in ft_state.toplevels.values_mut() {
        for (instance, outputs) in &mut data.instances {
            if let Some(client) = instance.client() {
                for wl_output in output.client_outputs(&client) {
                    if !outputs.iter().any(|o| o == &wl_output) {
                        instance.output_enter(&wl_output);
                        outputs.push(wl_output);
                    }
                }
                instance.done();
            }
        }
    }
}

/// Send output_leave for a disconnected output to all existing toplevels.
pub fn send_output_leave_all(
    ft_state: &mut ForeignToplevelManagerState,
    output: &Output,
) {
    for data in ft_state.toplevels.values_mut() {
        for (instance, outputs) in &mut data.instances {
            if let Some(client) = instance.client() {
                let client_outputs: Vec<_> = output.client_outputs(&client).collect();
                outputs.retain(|wl_output| {
                    if client_outputs.iter().any(|o| o == wl_output) {
                        instance.output_leave(wl_output);
                        false
                    } else {
                        true
                    }
                });
                if !client_outputs.is_empty() {
                    instance.done();
                }
            }
        }
    }
}

fn refresh_toplevel<D>(
    protocol_state: &mut ForeignToplevelManagerState,
    wl_surface: &WlSurface,
    outputs: &[Output],
    has_focus: bool,
) where
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()> + 'static,
{
    // Read title/app_id from xdg surface role data
    let (title, app_id, xdg_states) = with_states(wl_surface, |states| {
        let data = states.data_map.get::<XdgToplevelSurfaceData>();
        match data {
            Some(d) => {
                let guard = d.lock().unwrap();
                (guard.title.clone(), guard.app_id.clone(), guard.current.states.clone())
            }
            None => (None, None, Default::default()),
        }
    });

    let states = to_state_vec(&xdg_states, has_focus);

    match protocol_state.toplevels.entry(wl_surface.clone()) {
        Entry::Occupied(entry) => {
            let data = entry.into_mut();

            let mut new_title = None;
            if data.title != title {
                data.title.clone_from(&title);
                new_title = title.as_deref();
            }

            let mut new_app_id = None;
            if data.app_id != app_id {
                data.app_id.clone_from(&app_id);
                new_app_id = app_id.as_deref();
            }

            let mut states_changed = false;
            if data.states != states {
                data.states = states;
                states_changed = true;
            }

            let something_changed =
                new_title.is_some() || new_app_id.is_some() || states_changed;

            if something_changed {
                for instance in data.instances.keys() {
                    if let Some(new_title) = new_title {
                        instance.title(new_title.to_owned());
                    }
                    if let Some(new_app_id) = new_app_id {
                        instance.app_id(new_app_id.to_owned());
                    }
                    if states_changed {
                        instance.state(
                            data.states
                                .iter()
                                .flat_map(|x| x.to_ne_bytes())
                                .collect(),
                        );
                    }
                    instance.done();
                }
            }

            // Clean dead wl_outputs
            for wl_outputs in data.instances.values_mut() {
                wl_outputs.retain(|x| x.is_alive());
            }
        }
        Entry::Vacant(entry) => {
            // New window — send output_enter for ALL outputs
            let mut data = ToplevelData {
                title,
                app_id,
                states,
                instances: HashMap::new(),
            };

            for manager in &protocol_state.instances {
                if let Some(client) = manager.client() {
                    data.add_instance::<D>(
                        &protocol_state.display,
                        &client,
                        manager,
                        outputs,
                    );
                }
            }

            entry.insert(data);
        }
    }
}

impl ToplevelData {
    fn add_instance<D>(
        &mut self,
        handle: &DisplayHandle,
        client: &Client,
        manager: &ZwlrForeignToplevelManagerV1,
        all_outputs: &[Output],
    ) where
        D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
        D: 'static,
    {
        let toplevel = client
            .create_resource::<ZwlrForeignToplevelHandleV1, _, D>(handle, manager.version(), ())
            .unwrap();
        manager.toplevel(&toplevel);

        if let Some(title) = &self.title {
            toplevel.title(title.clone());
        }
        if let Some(app_id) = &self.app_id {
            toplevel.app_id(app_id.clone());
        }

        toplevel.state(self.states.iter().flat_map(|x| x.to_ne_bytes()).collect());

        // Canvas windows appear on all outputs
        let mut wl_outputs = Vec::new();
        for output in all_outputs {
            for wl_output in output.client_outputs(client) {
                toplevel.output_enter(&wl_output);
                wl_outputs.push(wl_output);
            }
        }

        toplevel.done();
        self.instances.insert(toplevel, wl_outputs);
    }
}

impl<D> GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData, D>
    for ForeignToplevelManagerState
where
    D: GlobalDispatch<ZwlrForeignToplevelManagerV1, ForeignToplevelGlobalData>,
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwlrForeignToplevelManagerV1>,
        _global_data: &ForeignToplevelGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());

        let outputs = state.foreign_toplevel_outputs();
        let ft_state = state.foreign_toplevel_manager_state();

        for data in ft_state.toplevels.values_mut() {
            data.add_instance::<D>(handle, client, &manager, &outputs);
        }

        ft_state.instances.push(manager);
    }

    fn can_view(client: Client, global_data: &ForeignToplevelGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ZwlrForeignToplevelManagerV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelManagerV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelManagerV1,
        request: <ZwlrForeignToplevelManagerV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_foreign_toplevel_manager_v1::Request::Stop => {
                resource.finished();
                let state = state.foreign_toplevel_manager_state();
                state.instances.retain(|x| x != resource);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelManagerV1,
        _data: &(),
    ) {
        let state = state.foreign_toplevel_manager_state();
        state.instances.retain(|x| x != resource);
    }
}

impl<D> Dispatch<ZwlrForeignToplevelHandleV1, (), D> for ForeignToplevelManagerState
where
    D: Dispatch<ZwlrForeignToplevelHandleV1, ()>,
    D: ForeignToplevelHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ZwlrForeignToplevelHandleV1,
        request: <ZwlrForeignToplevelHandleV1 as Resource>::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let protocol_state = state.foreign_toplevel_manager_state();

        let Some((surface, _)) = protocol_state
            .toplevels
            .iter()
            .find(|(_, data)| data.instances.contains_key(resource))
        else {
            return;
        };
        let surface = surface.clone();

        match request {
            zwlr_foreign_toplevel_handle_v1::Request::SetMaximized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMaximized => {
                // No-op: driftwm has no maximize (infinite canvas)
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetMinimized
            | zwlr_foreign_toplevel_handle_v1::Request::UnsetMinimized => {
                // No-op: no minimize concept
            }
            zwlr_foreign_toplevel_handle_v1::Request::Activate { .. } => {
                state.activate(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::Close => {
                state.close(surface);
            }
            zwlr_foreign_toplevel_handle_v1::Request::SetRectangle { .. } => {}
            zwlr_foreign_toplevel_handle_v1::Request::Destroy => {}
            zwlr_foreign_toplevel_handle_v1::Request::SetFullscreen { output } => {
                state.set_fullscreen(surface, output);
            }
            zwlr_foreign_toplevel_handle_v1::Request::UnsetFullscreen => {
                state.unset_fullscreen(surface);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ZwlrForeignToplevelHandleV1,
        _data: &(),
    ) {
        let state = state.foreign_toplevel_manager_state();
        for data in state.toplevels.values_mut() {
            data.instances.retain(|instance, _| instance != resource);
        }
    }
}

fn to_state_vec(
    states: &smithay::wayland::shell::xdg::ToplevelStateSet,
    has_focus: bool,
) -> Vec<u32> {
    let mut rv = Vec::with_capacity(3);
    if states.contains(xdg_toplevel::State::Maximized) {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Maximized as u32);
    }
    if states.contains(xdg_toplevel::State::Fullscreen) {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Fullscreen as u32);
    }
    if has_focus {
        rv.push(zwlr_foreign_toplevel_handle_v1::State::Activated as u32);
    }
    rv
}

#[macro_export]
macro_rules! delegate_foreign_toplevel {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: $crate::protocols::foreign_toplevel::ForeignToplevelGlobalData
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_manager_v1::ZwlrForeignToplevelManagerV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::foreign_toplevel::v1::server::zwlr_foreign_toplevel_handle_v1::ZwlrForeignToplevelHandleV1: ()
        ] => $crate::protocols::foreign_toplevel::ForeignToplevelManagerState);
    };
}
