//! Compositor keybindings for virtual-keyboard input.
//!
//! smithay's `zwp_virtual_keyboard_v1` implementation delivers key events
//! straight to the focused client, so an on-screen keyboard could never
//! trigger compositor bindings. These dispatch impls replace
//! `delegate_virtual_keyboard_manager!`: each key press first runs through
//! [`VirtualKeyboardBindingHandler::virtual_key_binding`] — resolved against
//! the virtual keyboard's *own* uploaded keymap and modifier state, which
//! need not match the physical layout — and a bound combo executes instead
//! of reaching the focused client (the paired release is swallowed too).
//! Everything else, including unbound keys, delegates to smithay's
//! implementation unchanged.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;

use smithay::input::SeatHandler;
use smithay::input::keyboard::{Keysym, ModifiersState, xkb};
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
    backend::{ClientId, ObjectId},
    protocol::wl_keyboard::KeymapFormat,
};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::virtual_keyboard::{
    VirtualKeyboardManagerGlobalData, VirtualKeyboardManagerState, VirtualKeyboardUserData,
};

pub trait VirtualKeyboardBindingHandler {
    fn virtual_keyboard_bindings(&mut self) -> &mut VirtualKeyboardBindings;

    /// Execute the compositor binding for `modifiers` + `sym`, if any.
    /// Returns `true` when a binding consumed the key press (it must not
    /// reach the focused client).
    fn virtual_key_binding(&mut self, modifiers: &ModifiersState, sym: Keysym) -> bool;
}

/// Per-virtual-keyboard xkb state, mirrored from the client's `keymap` and
/// `modifiers` requests (smithay keeps its own copy private). Keyed by the
/// `zwp_virtual_keyboard_v1` resource, so multiple virtual keyboards don't
/// mix layouts.
#[derive(Default)]
pub struct VirtualKeyboardBindings {
    keyboards: HashMap<ObjectId, VirtualKeyboard>,
}

struct VirtualKeyboard {
    state: xkb::State,
    /// Keycodes whose press a binding consumed; their release must be
    /// swallowed too, or the client sees a release without a press.
    swallowed: HashSet<u32>,
}

/// Far above any real xkb keymap (~100 KB), far below an allocation a hostile
/// `size` could weaponize — the wire value goes straight into a buffer.
const MAX_KEYMAP_SIZE: usize = 8 * 1024 * 1024;

impl VirtualKeyboardBindings {
    /// Number of tracked virtual keyboards (for leak diagnostics).
    pub fn keyboard_count(&self) -> usize {
        self.keyboards.len()
    }

    fn track_keymap(&mut self, id: ObjectId, format: u32, fd: &OwnedFd, size: usize) {
        if format != KeymapFormat::XkbV1 as u32 {
            return;
        }
        if size > MAX_KEYMAP_SIZE {
            tracing::warn!("virtual keyboard: keymap size {size} exceeds limit, ignoring");
            return;
        }
        // Dup the fd: the original stays with the request for smithay's own
        // keymap handling.
        let Ok(fd) = fd.try_clone() else {
            return;
        };
        let file = File::from(fd);
        let mut buf = vec![0u8; size];
        if file.read_exact_at(&mut buf, 0).is_err() {
            tracing::warn!("virtual keyboard: failed to read keymap fd");
            return;
        }
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let Ok(string) = std::str::from_utf8(&buf[..len]) else {
            tracing::warn!("virtual keyboard: keymap is not valid UTF-8");
            return;
        };
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let Some(keymap) = xkb::Keymap::new_from_string(
            &context,
            string.to_string(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        ) else {
            tracing::warn!("virtual keyboard: failed to compile keymap");
            return;
        };
        // A keymap re-upload (e.g. a layout switch) replaces the xkb state but
        // must keep the swallowed set: a key pressed under the old keymap still
        // owes its release a swallow.
        let swallowed = self
            .keyboards
            .remove(&id)
            .map(|kb| kb.swallowed)
            .unwrap_or_default();
        self.keyboards.insert(
            id,
            VirtualKeyboard {
                state: xkb::State::new(&keymap),
                swallowed,
            },
        );
    }

    fn track_modifiers(
        &mut self,
        id: ObjectId,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) {
        if let Some(kb) = self.keyboards.get_mut(&id) {
            kb.state
                .update_mask(depressed, latched, locked, 0, 0, group);
        }
    }
}

/// Resolve a virtual `key` event against the keyboard's mirrored xkb state and
/// hand it to the handler's binding lookup. Returns `true` when the event was
/// consumed (a bound press, or the release paired with one).
fn handle_key<D: VirtualKeyboardBindingHandler>(
    state: &mut D,
    id: ObjectId,
    key: u32,
    key_state: u32,
) -> bool {
    let Some(kb) = state.virtual_keyboard_bindings().keyboards.get_mut(&id) else {
        return false;
    };
    let pressed = key_state == 1;
    if !pressed {
        return kb.swallowed.remove(&key);
    }
    // Raw evdev keycode (wl_keyboard coding) → xkb keycode space.
    let sym = kb.state.key_get_one_sym(xkb::Keycode::new(key + 8));
    let effective = xkb::STATE_MODS_EFFECTIVE;
    let modifiers = ModifiersState {
        ctrl: kb.state.mod_name_is_active(xkb::MOD_NAME_CTRL, effective),
        alt: kb.state.mod_name_is_active(xkb::MOD_NAME_ALT, effective),
        shift: kb.state.mod_name_is_active(xkb::MOD_NAME_SHIFT, effective),
        logo: kb.state.mod_name_is_active(xkb::MOD_NAME_LOGO, effective),
        iso_level5_shift: kb.state.mod_name_is_active(xkb::MOD_NAME_MOD3, effective),
        ..Default::default()
    };
    if !state.virtual_key_binding(&modifiers, sym) {
        return false;
    }
    if let Some(kb) = state.virtual_keyboard_bindings().keyboards.get_mut(&id) {
        kb.swallowed.insert(key);
    }
    true
}

impl<D> GlobalDispatch<ZwpVirtualKeyboardManagerV1, VirtualKeyboardManagerGlobalData, D>
    for VirtualKeyboardBindings
where
    D: GlobalDispatch<ZwpVirtualKeyboardManagerV1, VirtualKeyboardManagerGlobalData>,
    D: Dispatch<ZwpVirtualKeyboardManagerV1, ()>,
    D: Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardUserData<D>>,
    D: SeatHandler + VirtualKeyboardBindingHandler + 'static,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        global_data: &VirtualKeyboardManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        <VirtualKeyboardManagerState as GlobalDispatch<
            ZwpVirtualKeyboardManagerV1,
            VirtualKeyboardManagerGlobalData,
            D,
        >>::bind(state, handle, client, resource, global_data, data_init);
    }

    fn can_view(client: Client, global_data: &VirtualKeyboardManagerGlobalData) -> bool {
        <VirtualKeyboardManagerState as GlobalDispatch<
            ZwpVirtualKeyboardManagerV1,
            VirtualKeyboardManagerGlobalData,
            D,
        >>::can_view(client, global_data)
    }
}

impl<D> Dispatch<ZwpVirtualKeyboardManagerV1, (), D> for VirtualKeyboardBindings
where
    D: Dispatch<ZwpVirtualKeyboardManagerV1, ()>,
    D: Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardUserData<D>>,
    D: SeatHandler + VirtualKeyboardBindingHandler + 'static,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
{
    fn request(
        state: &mut D,
        client: &Client,
        resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        data: &(),
        dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        <VirtualKeyboardManagerState as Dispatch<ZwpVirtualKeyboardManagerV1, (), D>>::request(
            state, client, resource, request, data, dhandle, data_init,
        );
    }
}

impl<D> Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardUserData<D>, D> for VirtualKeyboardBindings
where
    D: Dispatch<ZwpVirtualKeyboardV1, VirtualKeyboardUserData<D>>,
    D: SeatHandler + VirtualKeyboardBindingHandler + 'static,
    <D as SeatHandler>::KeyboardFocus: WaylandFocus,
{
    fn request(
        state: &mut D,
        client: &Client,
        resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        data: &VirtualKeyboardUserData<D>,
        dhandle: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match &request {
            zwp_virtual_keyboard_v1::Request::Keymap { format, fd, size } => {
                state.virtual_keyboard_bindings().track_keymap(
                    resource.id(),
                    *format,
                    fd,
                    *size as usize,
                );
            }
            zwp_virtual_keyboard_v1::Request::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                state.virtual_keyboard_bindings().track_modifiers(
                    resource.id(),
                    *mods_depressed,
                    *mods_latched,
                    *mods_locked,
                    *group,
                );
            }
            // The guard is where the work happens: `handle_key` runs the
            // binding lookup (and executes a matched action); a false guard
            // falls through to the delegation below like any other request.
            zwp_virtual_keyboard_v1::Request::Key {
                time: _,
                key,
                state: key_state,
            } if handle_key(state, resource.id(), *key, *key_state) => {
                return;
            }
            _ => {}
        }
        <VirtualKeyboardManagerState as Dispatch<
            ZwpVirtualKeyboardV1,
            VirtualKeyboardUserData<D>,
            D,
        >>::request(state, client, resource, request, data, dhandle, data_init);
    }

    fn destroyed(
        state: &mut D,
        client: ClientId,
        resource: &ZwpVirtualKeyboardV1,
        data: &VirtualKeyboardUserData<D>,
    ) {
        state
            .virtual_keyboard_bindings()
            .keyboards
            .remove(&resource.id());
        <VirtualKeyboardManagerState as Dispatch<
            ZwpVirtualKeyboardV1,
            VirtualKeyboardUserData<D>,
            D,
        >>::destroyed(state, client, resource, data);
    }
}

#[macro_export]
macro_rules! delegate_virtual_keyboard_bindings {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1: smithay::wayland::virtual_keyboard::VirtualKeyboardManagerGlobalData
        ] => $crate::protocols::virtual_keyboard::VirtualKeyboardBindings);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1: ()
        ] => $crate::protocols::virtual_keyboard::VirtualKeyboardBindings);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1: smithay::wayland::virtual_keyboard::VirtualKeyboardUserData<$ty>
        ] => $crate::protocols::virtual_keyboard::VirtualKeyboardBindings);
    };
}
