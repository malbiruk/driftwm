//! KDE server-side decoration protocol (`org_kde_kwin_server_decoration`).
//!
//! GTK3 does not support `zxdg_decoration_manager_v1` and instead negotiates
//! server-side decorations exclusively via this protocol. When a compositor
//! advertises `Mode::Server`, GTK suppresses its client-side title bar.

use std::sync::atomic::{AtomicU8, Ordering};

use smithay::reexports::wayland_protocols_misc::server_decoration::server::{
    org_kde_kwin_server_decoration::{self, OrgKdeKwinServerDecoration},
    org_kde_kwin_server_decoration_manager::{self, OrgKdeKwinServerDecorationManager},
};
use smithay::reexports::wayland_server::backend::{ClientId, GlobalId};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New,
};

const VERSION: u32 = 1;

pub struct ServerDecorationManagerState {
    #[allow(dead_code)]
    global: GlobalId,
}

pub struct ServerDecorationManagerGlobalData {
    filter: Box<dyn for<'a> Fn(&'a Client) -> bool + Send + Sync>,
}

pub struct ServerDecorationData {
    pub surface: WlSurface,
    pub requests_sent: AtomicU8,
}

pub trait ServerDecorationHandler {
    fn server_decoration_state(&mut self) -> &mut ServerDecorationManagerState;
    fn server_decoration_default_mode(&self) -> org_kde_kwin_server_decoration_manager::Mode;
    fn server_decoration_mode_for_surface(
        &self,
        surface: &WlSurface,
    ) -> org_kde_kwin_server_decoration::Mode;
}

impl ServerDecorationManagerState {
    pub fn new<D, F>(dh: &DisplayHandle, client_filter: F) -> Self
    where
        D: GlobalDispatch<OrgKdeKwinServerDecorationManager, ServerDecorationManagerGlobalData>
            + 'static,
        F: for<'a> Fn(&'a Client) -> bool + Send + Sync + 'static,
    {
        let global = dh.create_global::<D, OrgKdeKwinServerDecorationManager, _>(
            VERSION,
            ServerDecorationManagerGlobalData {
                filter: Box::new(client_filter),
            },
        );
        Self { global }
    }
}

impl<D> GlobalDispatch<OrgKdeKwinServerDecorationManager, ServerDecorationManagerGlobalData, D>
    for ServerDecorationManagerState
where
    D: GlobalDispatch<OrgKdeKwinServerDecorationManager, ServerDecorationManagerGlobalData>
        + Dispatch<OrgKdeKwinServerDecorationManager, ()>
        + ServerDecorationHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<OrgKdeKwinServerDecorationManager>,
        _global_data: &ServerDecorationManagerGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let default_mode = state.server_decoration_default_mode();
        let manager = data_init.init(resource, ());
        manager.default_mode(default_mode);
    }

    fn can_view(client: Client, global_data: &ServerDecorationManagerGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<OrgKdeKwinServerDecorationManager, (), D> for ServerDecorationManagerState
where
    D: Dispatch<OrgKdeKwinServerDecorationManager, ()>
        + Dispatch<OrgKdeKwinServerDecoration, ServerDecorationData>
        + ServerDecorationHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _obj: &OrgKdeKwinServerDecorationManager,
        request: org_kde_kwin_server_decoration_manager::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            org_kde_kwin_server_decoration_manager::Request::Create { id, surface } => {
                let mode = state.server_decoration_mode_for_surface(&surface);
                let deco = data_init.init(
                    id,
                    ServerDecorationData {
                        surface,
                        requests_sent: AtomicU8::new(0),
                    },
                );
                deco.mode(mode);
            }
            _ => unreachable!(),
        }
    }
}

impl<D> Dispatch<OrgKdeKwinServerDecoration, ServerDecorationData, D>
    for ServerDecorationManagerState
where
    D: Dispatch<OrgKdeKwinServerDecoration, ServerDecorationData>
        + ServerDecorationHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        obj: &OrgKdeKwinServerDecoration,
        request: org_kde_kwin_server_decoration::Request,
        data: &ServerDecorationData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            org_kde_kwin_server_decoration::Request::Release => {}
            org_kde_kwin_server_decoration::Request::RequestMode { mode: _ } => {
                let sent = data.requests_sent.fetch_add(1, Ordering::Relaxed);
                if sent > 3 {
                    return;
                }
                let mode = state.server_decoration_mode_for_surface(&data.surface);
                obj.mode(mode);
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut D,
        _client: ClientId,
        _obj: &OrgKdeKwinServerDecoration,
        _data: &ServerDecorationData,
    ) {
    }
}

#[macro_export]
macro_rules! delegate_server_decoration {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager: $crate::protocols::server_decoration::ServerDecorationManagerGlobalData
        ] => $crate::protocols::server_decoration::ServerDecorationManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration_manager::OrgKdeKwinServerDecorationManager: ()
        ] => $crate::protocols::server_decoration::ServerDecorationManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_misc::server_decoration::server::org_kde_kwin_server_decoration::OrgKdeKwinServerDecoration: $crate::protocols::server_decoration::ServerDecorationData
        ] => $crate::protocols::server_decoration::ServerDecorationManagerState);
    };
}
