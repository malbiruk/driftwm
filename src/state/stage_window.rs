//! The stage element type: a live client window or a suspended stand-in.
//!
//! `StageWindow` presents a `Window`-shaped facade (`geometry()`,
//! `wl_surface()`, `toplevel()`, …) so stage consumers stay mechanical; the
//! `Suspended` arms answer "no surface" and existing guards do the right
//! thing.

use std::cell::Cell;
use std::rc::Rc;

use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Rectangle, Size};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::ToplevelSurface;

use driftwm::stage::StageElement;
use driftwm::window_ext::WindowExt;

/// Durable session-record key for a suspended window. Distinct from the
/// per-process stage `ElementId`: this one survives compositor restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[allow(dead_code)]
pub struct SuspendedId(pub u64);

/// The `.desktop`-resolved identity of a suspended window's application.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AppIdentity {
    /// Original surface app_id — the matching key.
    pub app_id: String,
    /// Resolved `.desktop` id — the launch key.
    pub desktop_id: String,
    /// `Name=` from the entry.
    pub display_name: String,
}

/// A window kept on the canvas after its client is gone: size and identity,
/// no surface. Its canvas position lives in the stage entry, like any other
/// element's.
#[derive(Debug)]
#[allow(dead_code)]
pub struct SuspendedWindow {
    pub id: SuspendedId,
    pub size: Cell<Size<i32, Logical>>,
    pub identity: AppIdentity,
    /// Kept for IPC inventories only.
    pub last_title: String,
}

#[derive(Debug, Clone)]
pub enum StageWindow {
    Client(Window),
    /// Built by the suspend/resume flow, outside this crate's own call graph.
    #[allow(dead_code)]
    Suspended(Rc<SuspendedWindow>),
}

impl PartialEq for StageWindow {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Client(a), Self::Client(b)) => a == b,
            (Self::Suspended(a), Self::Suspended(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl PartialEq<Window> for StageWindow {
    fn eq(&self, other: &Window) -> bool {
        matches!(self, Self::Client(w) if w == other)
    }
}

impl From<Window> for StageWindow {
    fn from(window: Window) -> Self {
        Self::Client(window)
    }
}

impl StageWindow {
    /// The live client window, if this element has one.
    pub fn client(&self) -> Option<&Window> {
        match self {
            Self::Client(w) => Some(w),
            Self::Suspended(_) => None,
        }
    }

    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        match self {
            Self::Client(w) => w.geometry(),
            Self::Suspended(s) => Rectangle::from_size(s.size.get()),
        }
    }

    pub fn toplevel(&self) -> Option<&ToplevelSurface> {
        match self {
            Self::Client(w) => w.toplevel(),
            Self::Suspended(_) => None,
        }
    }

    pub fn set_activated(&self, active: bool) -> bool {
        match self {
            Self::Client(w) => w.set_activated(active),
            Self::Suspended(_) => false,
        }
    }
}

impl WaylandFocus for StageWindow {
    fn wl_surface(&self) -> Option<std::borrow::Cow<'_, WlSurface>> {
        match self {
            Self::Client(w) => w.wl_surface(),
            Self::Suspended(_) => None,
        }
    }
}

impl IsAlive for StageWindow {
    fn alive(&self) -> bool {
        match self {
            Self::Client(w) => w.alive(),
            // No surface to die: suspended windows leave only on dismissal.
            Self::Suspended(_) => true,
        }
    }
}

impl WindowExt for StageWindow {
    fn send_close(&self) {
        if let Self::Client(w) = self {
            w.send_close();
        }
    }

    fn app_id_or_class(&self) -> Option<String> {
        match self {
            Self::Client(w) => w.app_id_or_class(),
            Self::Suspended(s) => Some(s.identity.app_id.clone()),
        }
    }

    fn window_title(&self) -> Option<String> {
        match self {
            Self::Client(w) => w.window_title(),
            Self::Suspended(s) => Some(s.last_title.clone()),
        }
    }

    fn wants_ssd(&self) -> bool {
        match self {
            Self::Client(w) => w.wants_ssd(),
            Self::Suspended(_) => false,
        }
    }

    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>) {
        if let Self::Client(w) = self {
            w.enter_fullscreen_configure(size);
        }
    }

    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>) {
        if let Self::Client(w) = self {
            w.exit_fullscreen_configure(saved_size);
        }
    }

    fn enter_fit_configure(&self, size: Size<i32, Logical>) {
        if let Self::Client(w) = self {
            w.enter_fit_configure(size);
        }
    }

    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>) {
        if let Self::Client(w) = self {
            w.exit_fit_configure(saved_size);
        }
    }

    fn parent_surface(&self) -> Option<WlSurface> {
        match self {
            Self::Client(w) => w.parent_surface(),
            Self::Suspended(_) => None,
        }
    }

    fn is_modal(&self) -> bool {
        match self {
            Self::Client(w) => w.is_modal(),
            Self::Suspended(_) => false,
        }
    }

    fn is_widget(&self) -> bool {
        match self {
            Self::Client(w) => WindowExt::is_widget(w),
            Self::Suspended(_) => false,
        }
    }
}

impl StageElement for StageWindow {
    fn size(&self) -> Size<i32, Logical> {
        match self {
            Self::Client(w) => StageElement::size(w),
            Self::Suspended(s) => s.size.get(),
        }
    }

    fn is_alive(&self) -> bool {
        IsAlive::alive(self)
    }

    fn is_child_of(&self, parent: &Self) -> bool {
        match (self, parent) {
            (Self::Client(w), Self::Client(p)) => StageElement::is_child_of(w, p),
            _ => false,
        }
    }
}
