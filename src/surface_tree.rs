use smithay::{
    desktop::PopupManager,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::{compositor::get_parent, seat::WaylandFocus},
};

pub fn focus_belongs_to_window<W: WaylandFocus>(surface: &WlSurface, window: &W) -> bool {
    let Some(root) = window.wl_surface() else {
        return false;
    };

    focus_belongs_to_toplevel(surface, &root)
}

pub fn focus_belongs_to_toplevel(focus: &WlSurface, toplevel: &WlSurface) -> bool {
    surface_in_tree(focus, toplevel)
        || PopupManager::popups_for_surface(toplevel)
            .any(|(popup, _)| surface_in_tree(focus, popup.wl_surface()))
}

fn surface_in_tree(surface: &WlSurface, root: &WlSurface) -> bool {
    if surface == root {
        return true;
    }

    let mut current = surface.clone();
    while let Some(parent) = get_parent(&current) {
        if &parent == root {
            return true;
        }
        current = parent;
    }

    false
}
