//! `ext-workspace-v1` (staging): exports driftwm bookmarks to bars and shells
//! through the workspace metaphor. One group represents the whole canvas; each
//! bookmark is a workspace named after it. The single `active` state bit tracks
//! the focused viewport's nearest visible bookmark.
//!
//! This is an *export of bookmarks*, not real workspaces — no window↔workspace
//! association exists or is implied. smithay ships no helper for this protocol,
//! so it is hand-rolled against the generated interfaces, diffing the registry
//! per frame like `foreign_toplevel`.

use std::collections::{BTreeMap, BTreeSet};

use smithay::output::Output;
use smithay::reexports::wayland_protocols::ext::workspace::v1::server::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, State, WorkspaceCapabilities},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

const VERSION: u32 = 1;

/// One bound client: its manager, the single group handle, the per-bookmark
/// workspace handles, the outputs already advertised via `output_enter`, and
/// the double-buffered requests awaiting `commit`.
///
/// `group` is `None` once the client destroys the group handle while keeping
/// the manager live: workspaces, state, and `done` keep flowing, but
/// group-scoped events (output/workspace enter/leave) are then skipped.
struct ManagerInstance {
    manager: ExtWorkspaceManagerV1,
    group: Option<ExtWorkspaceGroupHandleV1>,
    workspaces: BTreeMap<String, ExtWorkspaceHandleV1>,
    outputs: Vec<WlOutput>,
    pending: Vec<PendingOp>,
}

/// A request accumulated on the manager between `commit`s (the protocol
/// double-buffers). Applied atomically when the client commits.
enum PendingOp {
    Activate(String),
    Create(String),
    Remove(String),
}

pub struct ExtWorkspaceManagerState {
    display: DisplayHandle,
    instances: Vec<ManagerInstance>,
    /// Bookmark names last advertised as workspaces — the diff baseline for
    /// `refresh` and the hydration list for `bind`.
    workspaces: BTreeSet<String>,
    /// Name of the workspace currently carrying the `active` bit (the focused
    /// output's incumbent), diffed each refresh to emit state flips.
    active: Option<String>,
}

pub struct ExtWorkspaceGlobalData {
    filter: Box<dyn for<'c> Fn(&'c Client) -> bool + Send + Sync>,
}

pub trait ExtWorkspaceHandler {
    fn ext_workspace_state(&mut self) -> &mut ExtWorkspaceManagerState;
    fn ext_workspace_outputs(&self) -> Vec<Output>;
    fn workspace_activate(&mut self, name: String);
    fn workspace_create(&mut self, name: String);
    fn workspace_remove(&mut self, name: String);
}

impl ExtWorkspaceManagerState {
    pub fn new<D, F>(display: &DisplayHandle, filter: F) -> Self
    where
        D: GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData>,
        D: Dispatch<ExtWorkspaceManagerV1, ()>,
        D: Dispatch<ExtWorkspaceGroupHandleV1, ()>,
        D: Dispatch<ExtWorkspaceHandleV1, ()>,
        D: ExtWorkspaceHandler,
        D: 'static,
        F: for<'c> Fn(&'c Client) -> bool + Send + Sync + 'static,
    {
        let global_data = ExtWorkspaceGlobalData {
            filter: Box::new(filter),
        };
        display.create_global::<D, ExtWorkspaceManagerV1, _>(VERSION, global_data);
        Self {
            display: display.clone(),
            instances: Vec::new(),
            workspaces: BTreeSet::new(),
            active: None,
        }
    }
}

/// Create a workspace handle for one bookmark on one instance and send its
/// initial burst: `workspace` (manager) → `id` → `name` → `capabilities` →
/// `state` → `workspace_enter` (group). The protocol requires `state` among the
/// initial details, so every workspace starts at `empty`; the caller re-emits
/// `state(active)` on the active one within the same atomic `done`.
fn create_workspace_handle<D>(
    display: &DisplayHandle,
    client: &Client,
    inst: &mut ManagerInstance,
    name: &str,
) where
    D: Dispatch<ExtWorkspaceHandleV1, ()> + 'static,
{
    let Ok(handle) =
        client.create_resource::<ExtWorkspaceHandleV1, _, D>(display, inst.manager.version(), ())
    else {
        return;
    };
    inst.manager.workspace(&handle);
    // The registry key is unique and stable, so it doubles as the protocol id.
    handle.id(name.to_owned());
    handle.name(name.to_owned());
    handle.capabilities(WorkspaceCapabilities::Activate | WorkspaceCapabilities::Remove);
    handle.state(State::empty());
    if let Some(group) = &inst.group {
        group.workspace_enter(&handle);
    }
    inst.workspaces.insert(name.to_owned(), handle);
}

/// Diff the bookmark registry against the advertised set and reconcile every
/// bound client: create handles for new bookmarks, `removed` gone ones, flip
/// the `active` state bit when the focused output's incumbent changes, and
/// reconcile the group's `output_enter`/`output_leave` against the wl_outputs
/// each client has bound. `active` is the focused output's incumbent — the only
/// output whose incumbent the protocol projects. `outputs` are the live outputs
/// (those with a wl_output global). Emits `done` per instance only when it
/// changed. Runs every frame: the manager global predates every wl_output
/// global and clients bind outputs on their own schedule, so output_enter can
/// only be delivered by diffing per frame, as the protocol requires.
pub fn refresh<D>(
    ws_state: &mut ExtWorkspaceManagerState,
    bookmarks: &BTreeMap<String, [f64; 2]>,
    active: Option<&str>,
    outputs: &[Output],
) where
    D: Dispatch<ExtWorkspaceHandleV1, ()> + 'static,
{
    let live: BTreeSet<String> = bookmarks.keys().cloned().collect();
    let created: Vec<String> = live.difference(&ws_state.workspaces).cloned().collect();
    let removed: Vec<String> = ws_state.workspaces.difference(&live).cloned().collect();
    let active_changed = ws_state.active.as_deref() != active;

    let old_active = ws_state.active.clone();
    let display = ws_state.display.clone();
    for inst in &mut ws_state.instances {
        let Some(client) = inst.manager.client() else {
            continue;
        };
        let mut changed = false;

        // Reconcile group outputs only while the group handle is live.
        if let Some(group) = inst.group.clone() {
            // The distinct wl_output resources this client has bound across the
            // live outputs. Each distinct resource gets its own enter; the
            // dedup only guards against the same resource being listed twice.
            let mut current: Vec<WlOutput> = Vec::new();
            for output in outputs {
                for wl_output in output.client_outputs(&client) {
                    if !current.contains(&wl_output) {
                        current.push(wl_output);
                    }
                }
            }
            for wl_output in &current {
                if !inst.outputs.contains(wl_output) {
                    group.output_enter(wl_output);
                    inst.outputs.push(wl_output.clone());
                    changed = true;
                }
            }
            inst.outputs.retain(|wl_output| {
                if !wl_output.is_alive() {
                    // Client released the proxy: drop it, but never send a leave
                    // to a dead wl_output (segfault hazard).
                    return false;
                }
                if current.contains(wl_output) {
                    true
                } else {
                    group.output_leave(wl_output);
                    changed = true;
                    false
                }
            });
        }

        for name in &removed {
            if let Some(handle) = inst.workspaces.remove(name) {
                // A workspace must leave its group before removal — the protocol
                // only removes workspaces belonging to no group.
                if let Some(group) = &inst.group {
                    group.workspace_leave(&handle);
                }
                handle.removed();
                changed = true;
            }
        }
        for name in &created {
            create_workspace_handle::<D>(&display, &client, inst, name);
            changed = true;
        }
        if active_changed {
            // Clearing the old handle is skipped when it was just removed above
            // (its entry is already gone), so no event targets a dead handle.
            if let Some(old) = old_active.as_deref()
                && let Some(handle) = inst.workspaces.get(old)
            {
                handle.state(State::empty());
                changed = true;
            }
            if let Some(new) = active
                && let Some(handle) = inst.workspaces.get(new)
            {
                handle.state(State::Active);
                changed = true;
            }
        }
        if changed {
            inst.manager.done();
        }
    }

    ws_state.workspaces = live;
    ws_state.active = active.map(str::to_owned);
}

/// Retract a disconnecting output from every bound client's group via
/// `output_leave`. MUST run before the caller disables the wl_output global — a
/// leave sent after teardown carries a NULL wl_output and segfaults clients that
/// don't null-check it.
pub fn send_output_leave(ws_state: &mut ExtWorkspaceManagerState, output: &Output) {
    for inst in &mut ws_state.instances {
        let Some(client) = inst.manager.client() else {
            continue;
        };
        let Some(group) = inst.group.clone() else {
            continue;
        };
        let client_outputs: Vec<_> = output.client_outputs(&client).collect();
        let mut changed = false;
        inst.outputs.retain(|wl_output| {
            if client_outputs.iter().any(|o| o == wl_output) {
                if wl_output.is_alive() {
                    group.output_leave(wl_output);
                    changed = true;
                }
                false
            } else {
                true
            }
        });
        if changed {
            inst.manager.done();
        }
    }
}

impl<D> GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData, D>
    for ExtWorkspaceManagerState
where
    D: GlobalDispatch<ExtWorkspaceManagerV1, ExtWorkspaceGlobalData>,
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: Dispatch<ExtWorkspaceGroupHandleV1, ()>,
    D: Dispatch<ExtWorkspaceHandleV1, ()>,
    D: ExtWorkspaceHandler,
    D: 'static,
{
    fn bind(
        state: &mut D,
        handle: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &ExtWorkspaceGlobalData,
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let outputs = state.ext_workspace_outputs();
        let ws_state = state.ext_workspace_state();

        let Ok(group) = client.create_resource::<ExtWorkspaceGroupHandleV1, _, D>(
            handle,
            manager.version(),
            (),
        ) else {
            // Without a group nothing works; don't track a half-built instance.
            manager.done();
            return;
        };
        manager.workspace_group(&group);
        group.capabilities(GroupCapabilities::CreateWorkspace);

        let mut inst = ManagerInstance {
            manager: manager.clone(),
            group: Some(group.clone()),
            workspaces: BTreeMap::new(),
            outputs: Vec::new(),
            pending: Vec::new(),
        };

        for output in &outputs {
            for wl_output in output.client_outputs(client) {
                group.output_enter(&wl_output);
                inst.outputs.push(wl_output);
            }
        }

        let names: Vec<String> = ws_state.workspaces.iter().cloned().collect();
        for name in &names {
            create_workspace_handle::<D>(handle, client, &mut inst, name);
        }
        if let Some(active) = ws_state.active.as_deref()
            && let Some(workspace) = inst.workspaces.get(active)
        {
            workspace.state(State::Active);
        }

        manager.done();
        ws_state.instances.push(inst);
    }

    fn can_view(client: Client, global_data: &ExtWorkspaceGlobalData) -> bool {
        (global_data.filter)(&client)
    }
}

impl<D> Dispatch<ExtWorkspaceManagerV1, (), D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceManagerV1, ()>,
    D: ExtWorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let ws_state = state.ext_workspace_state();
                let Some(inst) = ws_state
                    .instances
                    .iter_mut()
                    .find(|i| &i.manager == resource)
                else {
                    return;
                };
                let ops = std::mem::take(&mut inst.pending);
                for op in ops {
                    match op {
                        PendingOp::Activate(name) => state.workspace_activate(name),
                        PendingOp::Create(name) => state.workspace_create(name),
                        PendingOp::Remove(name) => state.workspace_remove(name),
                    }
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                resource.finished();
                state
                    .ext_workspace_state()
                    .instances
                    .retain(|i| &i.manager != resource);
            }
            other => {
                tracing::debug!("ext_workspace_manager_v1: ignoring unknown request {other:?}")
            }
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &ExtWorkspaceManagerV1, _data: &()) {
        state
            .ext_workspace_state()
            .instances
            .retain(|i| &i.manager != resource);
    }
}

impl<D> Dispatch<ExtWorkspaceGroupHandleV1, (), D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceGroupHandleV1, ()>,
    D: ExtWorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            ext_workspace_group_handle_v1::Request::CreateWorkspace { workspace } => {
                let ws_state = state.ext_workspace_state();
                if let Some(inst) = ws_state
                    .instances
                    .iter_mut()
                    .find(|i| i.group.as_ref() == Some(resource))
                {
                    inst.pending.push(PendingOp::Create(workspace));
                }
            }
            // Destroy is a destructor — the tombstone happens in `destroyed`.
            ext_workspace_group_handle_v1::Request::Destroy => {}
            other => {
                tracing::debug!("ext_workspace_group_handle_v1: ignoring unknown request {other:?}")
            }
        }
    }

    fn destroyed(
        state: &mut D,
        _client: ClientId,
        resource: &ExtWorkspaceGroupHandleV1,
        _data: &(),
    ) {
        // The client dropped the group but may keep the manager and workspaces:
        // tombstone only the group so its events stop while the rest flows.
        if let Some(inst) = state
            .ext_workspace_state()
            .instances
            .iter_mut()
            .find(|i| i.group.as_ref() == Some(resource))
        {
            inst.group = None;
        }
    }
}

impl<D> Dispatch<ExtWorkspaceHandleV1, (), D> for ExtWorkspaceManagerState
where
    D: Dispatch<ExtWorkspaceHandleV1, ()>,
    D: ExtWorkspaceHandler,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        _data: &(),
        _dhandle: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        let ws_state = state.ext_workspace_state();
        let found = ws_state.instances.iter().enumerate().find_map(|(i, inst)| {
            inst.workspaces
                .iter()
                .find_map(|(name, handle)| (handle == resource).then(|| name.clone()))
                .map(|name| (i, name))
        });
        let Some((idx, name)) = found else {
            return;
        };
        let inst = &mut ws_state.instances[idx];
        match request {
            ext_workspace_handle_v1::Request::Activate => {
                inst.pending.push(PendingOp::Activate(name));
            }
            ext_workspace_handle_v1::Request::Remove => {
                inst.pending.push(PendingOp::Remove(name));
            }
            // deactivate/assign are unsupported (capabilities omit them); accept
            // them as no-ops per protocol politeness.
            ext_workspace_handle_v1::Request::Deactivate
            | ext_workspace_handle_v1::Request::Assign { .. } => {}
            // Destroy is a destructor — cleanup happens in `destroyed`.
            ext_workspace_handle_v1::Request::Destroy => {}
            other => {
                tracing::debug!("ext_workspace_handle_v1: ignoring unknown request {other:?}")
            }
        }
    }

    fn destroyed(state: &mut D, _client: ClientId, resource: &ExtWorkspaceHandleV1, _data: &()) {
        for inst in &mut state.ext_workspace_state().instances {
            inst.workspaces.retain(|_, handle| handle != resource);
        }
    }
}

#[macro_export]
macro_rules! delegate_ext_workspace {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        smithay::reexports::wayland_server::delegate_global_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: $crate::protocols::ext_workspace::ExtWorkspaceGlobalData
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_manager_v1::ExtWorkspaceManagerV1: ()
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_group_handle_v1::ExtWorkspaceGroupHandleV1: ()
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
        smithay::reexports::wayland_server::delegate_dispatch!($(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)? $ty: [
            smithay::reexports::wayland_protocols::ext::workspace::v1::server::ext_workspace_handle_v1::ExtWorkspaceHandleV1: ()
        ] => $crate::protocols::ext_workspace::ExtWorkspaceManagerState);
    };
}
