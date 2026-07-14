//! `TestWindow` — a `Cell`-based mock window for stage unit tests and the
//! proptest harness. Identity is `Rc` pointer equality, like smithay's
//! `Window` (Arc identity).

use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Size};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::window_ext::WindowExt;

use super::StageElement;

/// Configure requests the stage/compositor sent to this window, recorded for
/// assertions. Real windows would answer these with a commit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SentConfigure {
    EnterFullscreen(Size<i32, Logical>),
    ExitFullscreen(Size<i32, Logical>),
    EnterFit(Size<i32, Logical>),
    ExitFit(Size<i32, Logical>),
}

#[derive(Default)]
struct Inner {
    label: u64,
    size: Cell<Size<i32, Logical>>,
    alive: Cell<bool>,
    /// Models a surfaceless suspended element: alive until dismissed
    /// (removed), regardless of the backing `alive` cell.
    suspended: Cell<bool>,
    widget: Cell<bool>,
    modal: Cell<bool>,
    parent: RefCell<Option<TestWindow>>,
    configures: RefCell<Vec<SentConfigure>>,
}

#[derive(Clone)]
pub struct TestWindow(Rc<Inner>);

impl PartialEq for TestWindow {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for TestWindow {}

impl std::hash::Hash for TestWindow {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Rc::as_ptr(&self.0) as usize).hash(state);
    }
}

impl std::fmt::Debug for TestWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TestWindow({})", self.0.label)
    }
}

impl TestWindow {
    pub fn new(label: u64) -> Self {
        let w = TestWindow(Rc::new(Inner {
            label,
            ..Default::default()
        }));
        w.0.alive.set(true);
        w.0.size.set(Size::from((100, 100)));
        w
    }

    /// A surfaceless suspended element (`StageWindow::Suspended` in
    /// production): permanently `is_alive`, so it survives `retain_alive`
    /// even after its former client dies.
    pub fn new_suspended(label: u64) -> Self {
        let w = Self::new(label);
        w.0.suspended.set(true);
        w
    }

    pub fn label(&self) -> u64 {
        self.0.label
    }

    pub fn is_suspended(&self) -> bool {
        self.0.suspended.get()
    }

    pub fn set_size(&self, size: Size<i32, Logical>) {
        self.0.size.set(size);
    }

    pub fn kill(&self) {
        self.0.alive.set(false);
    }

    pub fn set_widget(&self, widget: bool) {
        self.0.widget.set(widget);
    }

    pub fn set_modal(&self, modal: bool) {
        self.0.modal.set(modal);
    }

    pub fn set_parent(&self, parent: Option<TestWindow>) {
        *self.0.parent.borrow_mut() = parent;
    }

    pub fn parent(&self) -> Option<TestWindow> {
        self.0.parent.borrow().clone()
    }

    pub fn sent_configures(&self) -> Vec<SentConfigure> {
        self.0.configures.borrow().clone()
    }
}

impl WindowExt for TestWindow {
    fn send_close(&self) {}

    fn app_id_or_class(&self) -> Option<String> {
        Some(format!("test-{}", self.0.label))
    }

    fn window_title(&self) -> Option<String> {
        None
    }

    fn wants_ssd(&self) -> bool {
        false
    }

    fn enter_fullscreen_configure(&self, size: Size<i32, Logical>) {
        self.0
            .configures
            .borrow_mut()
            .push(SentConfigure::EnterFullscreen(size));
    }

    fn exit_fullscreen_configure(&self, saved_size: Size<i32, Logical>) {
        self.0
            .configures
            .borrow_mut()
            .push(SentConfigure::ExitFullscreen(saved_size));
    }

    fn enter_fit_configure(&self, size: Size<i32, Logical>) {
        self.0
            .configures
            .borrow_mut()
            .push(SentConfigure::EnterFit(size));
    }

    fn exit_fit_configure(&self, saved_size: Size<i32, Logical>) {
        self.0
            .configures
            .borrow_mut()
            .push(SentConfigure::ExitFit(saved_size));
    }

    fn parent_surface(&self) -> Option<WlSurface> {
        None
    }

    fn is_modal(&self) -> bool {
        self.0.modal.get()
    }

    fn is_widget(&self) -> bool {
        self.0.widget.get()
    }
}

impl StageElement for TestWindow {
    fn size(&self) -> Size<i32, Logical> {
        self.0.size.get()
    }

    fn is_alive(&self) -> bool {
        self.0.alive.get() || self.0.suspended.get()
    }

    fn is_child_of(&self, parent: &Self) -> bool {
        self.0.parent.borrow().as_ref() == Some(parent)
    }
}
