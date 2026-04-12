use std::collections::HashMap;
use std::mem;

use smithay::reexports::wayland_protocols_wlr::output_management::v1::server::{
    zwlr_output_configuration_head_v1, zwlr_output_configuration_v1, zwlr_output_head_v1,
    zwlr_output_manager_v1, zwlr_output_mode_v1,
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::Transform as WlTransform;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};
use smithay::utils::Transform;
use zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1;
use zwlr_output_configuration_v1::ZwlrOutputConfigurationV1;
use zwlr_output_head_v1::ZwlrOutputHeadV1;
use zwlr_output_manager_v1::ZwlrOutputManagerV1;
use zwlr_output_mode_v1::ZwlrOutputModeV1;

const VERSION: u32 = 4;

/// Snapshot of one output's current state.
pub struct OutputHeadState {
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial_number: String,
    pub physical_size: (i32, i32),
    pub modes: Vec<ModeInfo>,
    pub current_mode_index: Option<usize>,
    pub position: (i32, i32),
    pub transform: Transform,
    pub scale: f64,
}

pub struct ModeInfo {
    pub width: i32,
    pub height: i32,
    pub refresh: i32, // mHz
    pub preferred: bool,
}

/// What the client requested for one head in an Apply.
pub struct RequestedHeadConfig {
    pub output_name: String,
    pub enabled: bool,
    pub mode_index: Option<usize>,
    pub custom_mode: Option<(i32, i32, i32)>,
    pub position: Option<(i32, i32)>,
    pub transform: Option<Transform>,
    pub scale: Option<f64>,
}

struct ClientData {
    manager: ZwlrOutputManagerV1,
    heads: HashMap<String, (ZwlrOutputHeadV1, Vec<ZwlrOutputModeV1>)>,
    confs: HashMap<ZwlrOutputConfigurationV1, ConfigState>,
}

enum ConfigState {
    Ongoing(HashMap<String, RequestedHeadConfig>),
    Finished,
}

/// Attached as user_data on ZwlrOutputConfigurationHeadV1 resources.
pub enum ConfigHeadData {
    Cancelled,
    Ok(String, ZwlrOutputConfigurationV1),
}

/// user_data on ZwlrOutputModeV1 — enables direct index lookup in SetMode.
pub struct ModeData {
    pub output_name: String,
    pub mode_index: usize,
}

pub struct OutputManagementState {
    display: DisplayHandle,
    serial: u32,
    clients: HashMap<ClientId, ClientData>,
    current_state: HashMap<String, OutputHeadState>,
}

pub struct OutputManagementGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait OutputManagementHandler {
    fn output_management_state(&mut self) -> &mut OutputManagementState;
    fn apply_output_config(&mut self, configs: Vec<RequestedHeadConfig>) -> bool;
}

impl OutputManagementState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
        D: Dispatch<ZwlrOutputManagerV1, ()>,
        D: Dispatch<ZwlrOutputHeadV1, String>,
        D: Dispatch<ZwlrOutputConfigurationV1, u32>,
        D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
        D: Dispatch<ZwlrOutputModeV1, ModeData>,
        D: OutputManagementHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = OutputManagementGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ZwlrOutputManagerV1, _>(VERSION, global_data);

        Self {
            display: display.clone(),
            clients: HashMap::new(),
            serial: 0,
            current_state: HashMap::new(),
        }
    }
}

/// Send updated output state to all clients. Takes split references.
pub fn notify_changes<D>(
    om: &mut OutputManagementState,
    new_state: HashMap<String, OutputHeadState>,
) where
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: 'static,
{
    // Destroy all old heads
    for client_data in om.clients.values_mut() {
        for (_name, (head, modes)) in client_data.heads.drain() {
            for mode in &modes {
                mode.finished();
            }
            head.finished();
        }
        for (conf, conf_state) in client_data.confs.drain() {
            if matches!(conf_state, ConfigState::Ongoing(_)) {
                conf.cancelled();
            }
        }
    }

    om.current_state = new_state;
    om.serial += 1;

    let display = om.display.clone();
    for client_data in om.clients.values_mut() {
        let Some(client) = client_data.manager.client() else {
            continue;
        };
        for (name, head_state) in &om.current_state {
            send_new_head::<D>(&display, &client, client_data, name, head_state);
        }
        client_data.manager.done(om.serial);
    }
}

fn send_new_head<D>(
    display: &DisplayHandle,
    client: &Client,
    client_data: &mut ClientData,
    output_name: &str,
    state: &OutputHeadState,
) where
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: 'static,
{
    let version = client_data.manager.version();

    let Ok(head) =
        client.create_resource::<ZwlrOutputHeadV1, _, D>(display, version, output_name.to_string())
    else {
        return;
    };
    client_data.manager.head(&head);

    head.name(state.name.clone());
    head.description(state.description.clone());
    head.physical_size(state.physical_size.0, state.physical_size.1);

    if head.version() >= zwlr_output_head_v1::EVT_MAKE_SINCE {
        head.make(state.make.clone());
    }
    if head.version() >= zwlr_output_head_v1::EVT_MODEL_SINCE {
        head.model(state.model.clone());
    }
    if head.version() >= zwlr_output_head_v1::EVT_SERIAL_NUMBER_SINCE
        && !state.serial_number.is_empty()
    {
        head.serial_number(state.serial_number.clone());
    }

    let mut wl_modes = Vec::with_capacity(state.modes.len());
    for (i, mode) in state.modes.iter().enumerate() {
        let Ok(wl_mode) = client.create_resource::<ZwlrOutputModeV1, _, D>(
            display,
            version,
            ModeData {
                output_name: output_name.to_string(),
                mode_index: i,
            },
        ) else {
            continue;
        };
        head.mode(&wl_mode);
        wl_mode.size(mode.width, mode.height);
        wl_mode.refresh(mode.refresh);
        if mode.preferred {
            wl_mode.preferred();
        }
        wl_modes.push(wl_mode);
    }

    head.enabled(1);
    if let Some(idx) = state.current_mode_index
        && let Some(wl_mode) = wl_modes.get(idx)
    {
        head.current_mode(wl_mode);
    }
    head.position(state.position.0, state.position.1);
    head.transform(transform_to_wl(state.transform));
    head.scale(state.scale);

    client_data
        .heads
        .insert(output_name.to_string(), (head, wl_modes));
}

fn transform_to_wl(t: Transform) -> WlTransform {
    match t {
        Transform::Normal => WlTransform::Normal,
        Transform::_90 => WlTransform::_90,
        Transform::_180 => WlTransform::_180,
        Transform::_270 => WlTransform::_270,
        Transform::Flipped => WlTransform::Flipped,
        Transform::Flipped90 => WlTransform::Flipped90,
        Transform::Flipped180 => WlTransform::Flipped180,
        Transform::Flipped270 => WlTransform::Flipped270,
    }
}

fn wl_to_transform(t: WlTransform) -> Option<Transform> {
    Some(match t {
        WlTransform::Normal => Transform::Normal,
        WlTransform::_90 => Transform::_90,
        WlTransform::_180 => Transform::_180,
        WlTransform::_270 => Transform::_270,
        WlTransform::Flipped => Transform::Flipped,
        WlTransform::Flipped90 => Transform::Flipped90,
        WlTransform::Flipped180 => Transform::Flipped180,
        WlTransform::Flipped270 => Transform::Flipped270,
        _ => return None,
    })
}

// 1. GlobalDispatch — bind

impl<D> GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn bind(
        state: &mut D,
        display: &DisplayHandle,
        client: &Client,
        manager: New<ZwlrOutputManagerV1>,
        _global_data: &OutputManagementGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(manager, ());
        let g_state = state.output_management_state();

        let mut client_data = ClientData {
            manager: manager.clone(),
            heads: HashMap::new(),
            confs: HashMap::new(),
        };

        for (name, head_state) in &g_state.current_state {
            send_new_head::<D>(display, client, &mut client_data, name, head_state);
        }

        g_state.clients.insert(client.id(), client_data);
        manager.done(g_state.serial);
    }

    fn can_view(client: Client, global_data: &OutputManagementGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

// 2. Dispatch<ZwlrOutputManagerV1> — CreateConfiguration, Stop

impl<D> Dispatch<ZwlrOutputManagerV1, (), D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _manager: &ZwlrOutputManagerV1,
        request: zwlr_output_manager_v1::Request,
        _data: &(),
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_manager_v1::Request::CreateConfiguration { id, serial } => {
                let g_state = state.output_management_state();
                let conf = data_init.init(id, serial);
                if let Some(client_data) = g_state.clients.get_mut(&client.id()) {
                    if serial != g_state.serial {
                        conf.cancelled();
                        client_data.confs.insert(conf, ConfigState::Finished);
                    } else {
                        client_data
                            .confs
                            .insert(conf, ConfigState::Ongoing(HashMap::new()));
                    }
                }
            }
            zwlr_output_manager_v1::Request::Stop => {
                let g_state = state.output_management_state();
                if let Some(c) = g_state.clients.remove(&client.id()) {
                    c.manager.finished();
                }
            }
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, client: ClientId, _resource: &ZwlrOutputManagerV1, _data: &()) {
        state.output_management_state().clients.remove(&client);
    }
}

// 3. Dispatch<ZwlrOutputConfigurationV1> — EnableHead, DisableHead, Apply, Test, Destroy

impl<D> Dispatch<ZwlrOutputConfigurationV1, u32, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        conf: &ZwlrOutputConfigurationV1,
        request: zwlr_output_configuration_v1::Request,
        serial: &u32,
        _display: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let outdated = *serial != g_state.serial;

        let new_config = g_state
            .clients
            .get_mut(&client.id())
            .and_then(|data| data.confs.get_mut(conf));

        match request {
            zwlr_output_configuration_v1::Request::EnableHead { id, head } => {
                let Some(output_name) = head.data::<String>() else {
                    let _fail = data_init.init(id, ConfigHeadData::Cancelled);
                    return;
                };
                if outdated {
                    let _fail = data_init.init(id, ConfigHeadData::Cancelled);
                    return;
                }
                let Some(new_config) = new_config else {
                    let _fail = data_init.init(id, ConfigHeadData::Cancelled);
                    return;
                };
                let ConfigState::Ongoing(heads) = new_config else {
                    let _fail = data_init.init(id, ConfigHeadData::Cancelled);
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };
                if heads.contains_key(output_name) {
                    let _fail = data_init.init(id, ConfigHeadData::Cancelled);
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head has been already configured",
                    );
                    return;
                }
                heads.insert(
                    output_name.clone(),
                    RequestedHeadConfig {
                        output_name: output_name.clone(),
                        enabled: true,
                        mode_index: None,
                        custom_mode: None,
                        position: None,
                        transform: None,
                        scale: None,
                    },
                );
                data_init.init(id, ConfigHeadData::Ok(output_name.clone(), conf.clone()));
            }
            zwlr_output_configuration_v1::Request::DisableHead { head } => {
                let Some(output_name) = head.data::<String>() else {
                    return;
                };
                if outdated {
                    return;
                }
                let Some(new_config) = new_config else {
                    return;
                };
                let ConfigState::Ongoing(heads) = new_config else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };
                if heads.contains_key(output_name) {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyConfiguredHead,
                        "head has been already configured",
                    );
                    return;
                }
                heads.insert(
                    output_name.clone(),
                    RequestedHeadConfig {
                        output_name: output_name.clone(),
                        enabled: false,
                        mode_index: None,
                        custom_mode: None,
                        position: None,
                        transform: None,
                        scale: None,
                    },
                );
            }
            zwlr_output_configuration_v1::Request::Apply => {
                if outdated {
                    conf.cancelled();
                    return;
                }
                let Some(new_config) = new_config else {
                    conf.failed();
                    return;
                };
                let ConfigState::Ongoing(heads) = mem::replace(new_config, ConfigState::Finished)
                else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                // Reject disable or mode changes (out of scope)
                let has_unsupported = heads.values().any(|cfg| {
                    !cfg.enabled || cfg.mode_index.is_some() || cfg.custom_mode.is_some()
                });
                if has_unsupported {
                    conf.failed();
                    return;
                }

                let configs: Vec<RequestedHeadConfig> = heads.into_values().collect();
                if state.apply_output_config(configs) {
                    conf.succeeded();
                } else {
                    conf.failed();
                }
            }
            zwlr_output_configuration_v1::Request::Test => {
                if outdated {
                    conf.cancelled();
                    return;
                }
                let Some(new_config) = new_config else {
                    conf.failed();
                    return;
                };
                let ConfigState::Ongoing(heads) = mem::replace(new_config, ConfigState::Finished)
                else {
                    conf.post_error(
                        zwlr_output_configuration_v1::Error::AlreadyUsed,
                        "configuration had already been used",
                    );
                    return;
                };

                let has_unsupported = heads.values().any(|cfg| {
                    !cfg.enabled || cfg.mode_index.is_some() || cfg.custom_mode.is_some()
                });
                if has_unsupported {
                    conf.failed();
                } else {
                    conf.succeeded();
                }
            }
            zwlr_output_configuration_v1::Request::Destroy => {
                g_state
                    .clients
                    .get_mut(&client.id())
                    .map(|d| d.confs.remove(conf));
            }
            _ => unreachable!(),
        }
    }
}

// 4. Dispatch<ZwlrOutputConfigurationHeadV1> — SetMode, SetCustomMode, SetPosition, SetTransform, SetScale

impl<D> Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        state: &mut D,
        client: &Client,
        _conf_head: &ZwlrOutputConfigurationHeadV1,
        request: zwlr_output_configuration_head_v1::Request,
        data: &ConfigHeadData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let g_state = state.output_management_state();
        let Some(client_data) = g_state.clients.get_mut(&client.id()) else {
            return;
        };
        let ConfigHeadData::Ok(output_name, conf) = data else {
            return;
        };
        let Some(conf_state) = client_data.confs.get_mut(conf) else {
            return;
        };
        let ConfigState::Ongoing(heads) = conf_state else {
            return;
        };
        let Some(head_cfg) = heads.get_mut(output_name) else {
            return;
        };

        match request {
            zwlr_output_configuration_head_v1::Request::SetMode { mode } => {
                if let Some(mode_data) = mode.data::<ModeData>() {
                    head_cfg.mode_index = Some(mode_data.mode_index);
                }
            }
            zwlr_output_configuration_head_v1::Request::SetCustomMode {
                width,
                height,
                refresh,
            } => {
                head_cfg.custom_mode = Some((width, height, refresh));
            }
            zwlr_output_configuration_head_v1::Request::SetPosition { x, y } => {
                head_cfg.position = Some((x, y));
            }
            zwlr_output_configuration_head_v1::Request::SetTransform {
                transform: WEnum::Value(t),
            } => {
                head_cfg.transform = wl_to_transform(t);
            }
            zwlr_output_configuration_head_v1::Request::SetScale { scale } => {
                if scale > 0.0 {
                    head_cfg.scale = Some(scale);
                }
            }
            zwlr_output_configuration_head_v1::Request::SetAdaptiveSync { .. } => {
                // VRR not supported
            }
            _ => {}
        }
    }
}

// 5. Dispatch<ZwlrOutputHeadV1> — Release

impl<D> Dispatch<ZwlrOutputHeadV1, String, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _head: &ZwlrOutputHeadV1,
        request: zwlr_output_head_v1::Request,
        _data: &String,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_head_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }

    fn destroyed(state: &mut D, client: ClientId, _resource: &ZwlrOutputHeadV1, data: &String) {
        if let Some(c) = state.output_management_state().clients.get_mut(&client) {
            c.heads.remove(data);
        }
    }
}

// 6. Dispatch<ZwlrOutputModeV1> — Release

impl<D> Dispatch<ZwlrOutputModeV1, ModeData, D> for OutputManagementState
where
    D: GlobalDispatch<ZwlrOutputManagerV1, OutputManagementGlobalData>,
    D: Dispatch<ZwlrOutputManagerV1, ()>,
    D: Dispatch<ZwlrOutputHeadV1, String>,
    D: Dispatch<ZwlrOutputConfigurationV1, u32>,
    D: Dispatch<ZwlrOutputConfigurationHeadV1, ConfigHeadData>,
    D: Dispatch<ZwlrOutputModeV1, ModeData>,
    D: OutputManagementHandler,
    D: 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _mode: &ZwlrOutputModeV1,
        request: zwlr_output_mode_v1::Request,
        _data: &ModeData,
        _display: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            zwlr_output_mode_v1::Request::Release => {}
            _ => unreachable!(),
        }
    }
}

// --- Delegate macro ---

#[macro_export]
macro_rules! delegate_output_management {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: $crate::protocols::output_management::OutputManagementGlobalData
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_manager_v1::ZwlrOutputManagerV1: ()
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_head_v1::ZwlrOutputHeadV1: String
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_v1::ZwlrOutputConfigurationV1: u32
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1: $crate::protocols::output_management::ConfigHeadData
        ] => $crate::protocols::output_management::OutputManagementState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols_wlr::output_management::v1::server::zwlr_output_mode_v1::ZwlrOutputModeV1: $crate::protocols::output_management::ModeData
        ] => $crate::protocols::output_management::OutputManagementState);
    };
}
