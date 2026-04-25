//! `hyprland-toplevel-export-v1` protocol implementation.
//!
//! Exposes `hyprland_toplevel_export_manager_v1` so that `xdg-desktop-portal-hyprland`
//! can capture individual window contents.  Window identity is resolved through the
//! `zwlr_foreign_toplevel_handle_v1` handles already tracked by `foreign_toplevel.rs`.
//!
//! Protocol flow:
//! 1. Client binds `hyprland_toplevel_export_manager_v1`.
//! 2. Client calls `capture_toplevel_with_wlr_toplevel_handle(frame, cursor, handle)`.
//! 3. Compositor replies with `frame.buffer(…)`, `frame.linux_dmabuf(…)`, `frame.buffer_done()`.
//! 4. Client allocates a matching buffer and calls `frame.copy(buffer, ignore_damage)`.
//! 5. Compositor renders the window and replies with `frame.flags(…)` + `frame.ready(ts)`.

#![allow(clippy::module_inception)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::backend::ObjectId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};
use smithay::utils::{Physical, Size};

// Protocol bindings generated at compile time by `wayland_scanner::generate_server_code!`.
// The explicit `wayland_server` sub-module is required because Rust 2024 edition no longer
// makes `use crate_name;` visible as `super::crate_name` inside nested modules — which is
// the exact path pattern the generated code emits.
pub mod proto {
    #![allow(
        unused_imports,
        dead_code,
        non_camel_case_types,
        clippy::all,
        rustdoc::all
    )]

    pub mod wayland_server {
        pub use ::wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle,
            GlobalDispatch, New, Resource, ResourceData, DispatchError, Weak,
        };
        // Some generated paths use `super::wayland_server::ObjectId` directly
        // (not through `::backend`), so re-export these here as well.
        // `super::wayland_server::ObjectId` is emitted directly by some scanner code paths.
        pub use ::wayland_server::backend::{ObjectId, InvalidId};
        pub mod backend {
            pub use ::wayland_server::backend::*;
        }
        pub mod protocol {
            pub use ::wayland_server::protocol::*;
            pub mod __interfaces {
                pub use ::wayland_server::protocol::__interfaces::*;
            }
        }
    }

    use self::wayland_server::protocol::*;

    pub mod __interfaces {
        use super::wayland_server::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!(
            "protocols/hyprland-toplevel-export-v1.xml"
        );
    }
    use self::__interfaces::*;

    wayland_scanner::generate_server_code!("protocols/hyprland-toplevel-export-v1.xml");
}

pub use proto::hyprland_toplevel_export_frame_v1::HyprlandToplevelExportFrameV1;
pub use proto::hyprland_toplevel_export_manager_v1::HyprlandToplevelExportManagerV1;

use proto::hyprland_toplevel_export_frame_v1 as frame_proto;
use proto::hyprland_toplevel_export_manager_v1 as manager_proto;

const VERSION: u32 = 2;

/// Pending capture frame waiting for the client to call `copy()`.
pub struct PendingToplevelExport {
    /// The frame resource the client holds.
    pub frame: HyprlandToplevelExportFrameV1,
    /// Identified window to capture.
    pub window: Window,
    /// Physical pixel size for the buffer (window geometry at output scale).
    pub buffer_size: Size<i32, Physical>,
    /// Whether to include the cursor in the capture.
    pub overlay_cursor: bool,
    /// WlBuffer provided by the client via `copy()`.
    pub buffer: WlBuffer,
}

/// Per-manager user data (just a client-visibility filter).
pub struct HyprlandExportManagerGlobalData {
    pub filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

/// Per-frame user data stored in `Dispatch`.
pub struct HyprlandExportFrameData {
    /// Window being captured.  `None` for invalid frames (e.g. v1 deprecated path).
    pub window: Option<Window>,
    pub buffer_size: Size<i32, Physical>,
    pub overlay_cursor: bool,
    /// Becomes true after the client calls `copy()` — prevents double-copy.
    pub copied: Arc<AtomicBool>,
}

/// Top-level compositor state for the hyprland toplevel export protocol.
#[derive(Default)]
pub struct HyprlandToplevelExportState;

impl HyprlandToplevelExportState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<HyprlandToplevelExportManagerV1, HyprlandExportManagerGlobalData>
            + Dispatch<HyprlandToplevelExportManagerV1, ()>
            + Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData>
            + HyprlandToplevelExportHandler
            + 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        display.create_global::<D, HyprlandToplevelExportManagerV1, _>(
            VERSION,
            HyprlandExportManagerGlobalData {
                filter: Box::new(filter),
            },
        );
        Self
    }
}

pub trait HyprlandToplevelExportHandler:
    GlobalDispatch<HyprlandToplevelExportManagerV1, HyprlandExportManagerGlobalData>
    + Dispatch<HyprlandToplevelExportManagerV1, ()>
    + Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData>
    + 'static
{
    /// Look up the `Window` for the given raw `ObjectId` of a
    /// `zwlr_foreign_toplevel_handle_v1` resource.  Returns `None` if the
    /// handle is not found or the window no longer exists.
    fn window_for_toplevel_handle(&self, handle_id: &ObjectId) -> Option<Window>;

    /// The compositor calls this when a `copy()` request arrives on a pending
    /// frame so the render loop knows to fulfill it.
    fn export_frame_ready(&mut self, export: PendingToplevelExport);
}

impl<D> GlobalDispatch<HyprlandToplevelExportManagerV1, HyprlandExportManagerGlobalData, D>
    for HyprlandToplevelExportState
where
    D: GlobalDispatch<HyprlandToplevelExportManagerV1, HyprlandExportManagerGlobalData>
        + Dispatch<HyprlandToplevelExportManagerV1, ()>
        + Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData>
        + HyprlandToplevelExportHandler
        + 'static,
{
    fn bind(
        _state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        manager: New<HyprlandToplevelExportManagerV1>,
        _global_data: &HyprlandExportManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        data_init.init(manager, ());
    }

    fn can_view(client: Client, global_data: &HyprlandExportManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<HyprlandToplevelExportManagerV1, (), D> for HyprlandToplevelExportState
where
    D: Dispatch<HyprlandToplevelExportManagerV1, ()>
        + Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData>
        + HyprlandToplevelExportHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _manager: &HyprlandToplevelExportManagerV1,
        request: manager_proto::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            manager_proto::Request::Destroy => {
                // destructor handled by wayland-server
            }

            // v1 deprecated request — init the frame as invalid and fail it immediately
            manager_proto::Request::CaptureToplevel { frame, .. } => {
                tracing::debug!("hyprland_toplevel_export: v1 CaptureToplevel not supported");
                let frame_obj = data_init.init(frame, HyprlandExportFrameData {
                    window: None,
                    buffer_size: Size::from((0, 0)),
                    overlay_cursor: false,
                    copied: Arc::new(AtomicBool::new(false)),
                });
                frame_obj.failed();
            }

            manager_proto::Request::CaptureToplevelWithWlrToplevelHandle {
                frame,
                overlay_cursor,
                handle,
            } => {
                // When `type="object"` has no `interface=` in the XML, the scanner
                // generates the argument as `ObjectId` directly — not as `Resource<T>`.
                let handle_id = handle;
                let window = state.window_for_toplevel_handle(&handle_id);

                if window.is_none() {
                    tracing::warn!(
                        "hyprland_toplevel_export: no window found for handle {handle_id:?}"
                    );
                }

                let (width, height) = window
                    .as_ref()
                    .map(|w| {
                        let geo = w.geometry();
                        (geo.size.w.max(1), geo.size.h.max(1))
                    })
                    .unwrap_or((1, 1));

                let buffer_size = Size::<i32, Physical>::from((width, height));
                let overlay_cursor = overlay_cursor != 0;

                let frame_obj = data_init.init(frame, HyprlandExportFrameData {
                    window,
                    buffer_size,
                    overlay_cursor,
                    copied: Arc::new(AtomicBool::new(false)),
                });

                if frame_obj.data::<HyprlandExportFrameData>().is_some_and(|d| d.window.is_none()) {
                    frame_obj.failed();
                    return;
                }
                frame_obj.buffer(
                    Format::Xrgb8888,
                    width as u32,
                    height as u32,
                    width as u32 * 4,
                );
                // Advertise DMA-BUF format
                frame_obj.linux_dmabuf(
                    smithay::backend::allocator::Fourcc::Xrgb8888 as u32,
                    width as u32,
                    height as u32,
                );
                frame_obj.buffer_done();
            }
        }
    }
}

impl<D> Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData, D>
    for HyprlandToplevelExportState
where
    D: Dispatch<HyprlandToplevelExportFrameV1, HyprlandExportFrameData>
        + HyprlandToplevelExportHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        frame: &HyprlandToplevelExportFrameV1,
        request: frame_proto::Request,
        data: &HyprlandExportFrameData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            frame_proto::Request::Destroy => {}

            frame_proto::Request::Copy { buffer, ignore_damage: _ } => {
                // Fail immediately for invalid frames (v1 or window-not-found)
                if data.window.is_none() {
                    frame.failed();
                    return;
                }

                // Guard against duplicate copy
                if data.copied.swap(true, Ordering::SeqCst) {
                    frame.post_error(
                        frame_proto::Error::AlreadyUsed,
                        "copy() called more than once on this frame",
                    );
                    return;
                }

                // Validate buffer size
                let size = data.buffer_size;
                if size.w <= 0 || size.h <= 0 {
                    frame.failed();
                    return;
                }

                // Validate SHM buffer dimensions if SHM
                let valid = if let Ok(_dmabuf) = smithay::wayland::dmabuf::get_dmabuf(&buffer) {
                    true // DMA-BUF size validation happens in the render path
                } else {
                    smithay::wayland::shm::with_buffer_contents(&buffer, |_, shm_len, bd| {
                        bd.format == Format::Xrgb8888
                            && bd.width == size.w
                            && bd.height == size.h
                            && bd.stride == size.w * 4
                            && shm_len == bd.stride as usize * bd.height as usize
                    })
                    .unwrap_or(false)
                };

                if !valid {
                    frame.post_error(
                        frame_proto::Error::InvalidBuffer,
                        "buffer format or size does not match the announced parameters",
                    );
                    return;
                }

                state.export_frame_ready(PendingToplevelExport {
                    frame: frame.clone(),
                    window: data.window.clone().unwrap(),
                    buffer_size: size,
                    overlay_cursor: data.overlay_cursor,
                    buffer,
                });
            }
        }
    }
}

/// Routes all `hyprland-toplevel-export-v1` dispatch to [`HyprlandToplevelExportState`].
/// Invoke once in the compositor's handler module alongside the other `delegate_*!` macros.
#[macro_export]
macro_rules! delegate_hyprland_toplevel_export {
    ($ty:ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($ty: [
            $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportManagerV1:
                $crate::protocols::hyprland_toplevel_export::HyprlandExportManagerGlobalData
        ] => $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportState);

        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportManagerV1: ()
        ] => $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportState);

        smithay::reexports::wayland_server::delegate_dispatch!($ty: [
            $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportFrameV1:
                $crate::protocols::hyprland_toplevel_export::HyprlandExportFrameData
        ] => $crate::protocols::hyprland_toplevel_export::HyprlandToplevelExportState);
    };
}
