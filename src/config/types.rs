use std::f64::consts::FRAC_1_SQRT_2;

use smithay::input::keyboard::ModifiersState;

pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;

#[derive(Clone, Debug, PartialEq)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
    UpLeft,
    UpRight,
    DownLeft,
    DownRight,
}

impl Direction {
    /// Normalized direction vector for this direction.
    pub fn to_unit_vec(&self) -> (f64, f64) {
        match self {
            Direction::Up => (0.0, -1.0),
            Direction::Down => (0.0, 1.0),
            Direction::Left => (-1.0, 0.0),
            Direction::Right => (1.0, 0.0),
            Direction::UpLeft => (-FRAC_1_SQRT_2, -FRAC_1_SQRT_2),
            Direction::UpRight => (FRAC_1_SQRT_2, -FRAC_1_SQRT_2),
            Direction::DownLeft => (-FRAC_1_SQRT_2, FRAC_1_SQRT_2),
            Direction::DownRight => (FRAC_1_SQRT_2, FRAC_1_SQRT_2),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    Exec(String),
    Spawn(String),
    CloseWindow,
    NudgeWindow(Direction),
    PanViewport(Direction),
    CenterWindow,
    CenterNearest(Direction),
    CycleWindows { backward: bool },
    HomeToggle,
    GoToPosition(f64, f64),
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ZoomToFit,
    ToggleFullscreen,
    ReloadConfig,
    Quit,
}

impl Action {
    /// Actions that should auto-repeat when their key is held.
    pub fn is_repeatable(&self) -> bool {
        matches!(
            self,
            Action::ZoomIn
                | Action::ZoomOut
                | Action::NudgeWindow(_)
                | Action::PanViewport(_)
                | Action::CycleWindows { .. }
                | Action::Spawn(_)
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl Modifiers {
    pub(crate) const EMPTY: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
    };

    pub(super) fn from_state(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
        }
    }
}

/// Which physical key acts as the window-manager modifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModKey {
    Alt,
    Super,
}

impl ModKey {
    /// Base modifier pattern with only the WM mod key set.
    pub(crate) fn base(self) -> Modifiers {
        match self {
            ModKey::Alt => Modifiers {
                alt: true,
                ..Modifiers::EMPTY
            },
            ModKey::Super => Modifiers {
                logo: true,
                ..Modifiers::EMPTY
            },
        }
    }

    /// Check if this mod key is pressed in the given modifier state.
    pub fn is_pressed(self, state: &ModifiersState) -> bool {
        match self {
            ModKey::Alt => state.alt,
            ModKey::Super => state.logo,
        }
    }
}

/// Which modifier must be held during window cycling (Alt-Tab style).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CycleModifier {
    Alt,
    Ctrl,
}

impl CycleModifier {
    pub fn is_pressed(self, state: &ModifiersState) -> bool {
        match self {
            CycleModifier::Alt => state.alt,
            CycleModifier::Ctrl => state.ctrl,
        }
    }

    pub(crate) fn base(self) -> Modifiers {
        match self {
            CycleModifier::Alt => Modifiers {
                alt: true,
                ..Modifiers::EMPTY
            },
            CycleModifier::Ctrl => Modifiers {
                ctrl: true,
                ..Modifiers::EMPTY
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub modifiers: Modifiers,
    pub sym: smithay::input::keyboard::Keysym,
}

impl KeyCombo {
    /// Normalize keysym quirks so bindings match intuitively:
    /// - Uppercase letters (A-Z) → lowercase (a-z), Shift untouched
    /// - ISO_Left_Tab → Tab + Shift (XKB emits ISO_Left_Tab for Shift+Tab)
    pub fn normalize(&mut self) {
        use smithay::input::keyboard::keysyms;
        let raw = self.sym.raw();
        if (0x41..=0x5a).contains(&raw) {
            self.sym = smithay::input::keyboard::Keysym::from(raw + 0x20);
        } else if raw == keysyms::KEY_ISO_Left_Tab {
            self.sym = smithay::input::keyboard::Keysym::from(keysyms::KEY_Tab);
            self.modifiers.shift = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MouseTrigger {
    Button(u32),
    Scroll,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MouseBinding {
    pub modifiers: Modifiers,
    pub trigger: MouseTrigger,
}

#[derive(Clone, Debug)]
pub enum MouseAction {
    MoveWindow,
    ResizeWindow,
    PanViewport,
    Zoom,
    Navigate,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackpadSettings {
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub tap_and_drag: bool,
    pub accel_speed: f64,
}

impl Default for TrackpadSettings {
    fn default() -> Self {
        Self {
            tap_to_click: true,
            natural_scroll: true,
            tap_and_drag: true,
            accel_speed: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyboardLayout {
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub model: String,
}

/// Decoration mode applied by a window rule.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DecorationMode {
    /// Client-side decorations (default — compositor advertises CSD-first).
    #[default]
    Client,
    /// Server-side decorations (compositor draws frame — currently renders nothing = borderless).
    Server,
    /// No decorations at all: force SSD mode but draw nothing.
    None,
}

/// Parsed window rule from config.
#[derive(Clone, Debug)]
pub struct WindowRule {
    pub app_id: String,
    pub position: Option<(i32, i32)>,
    /// Widget windows are pinned (immovable), excluded from navigation/alt-tab,
    /// and always stacked below normal windows.
    pub widget: bool,
    pub no_focus: bool,
    pub decoration: DecorationMode,
}

/// Runtime rule state stored in a surface's data_map after matching.
#[derive(Clone, Debug)]
pub struct AppliedWindowRule {
    pub widget: bool,
    pub no_focus: bool,
    pub decoration: DecorationMode,
}

/// Read the applied window rule from a surface's data_map (if any).
pub fn applied_rule(
    surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> Option<AppliedWindowRule> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<std::sync::Mutex<AppliedWindowRule>>()
            .and_then(|m| m.lock().ok())
            .map(|guard| guard.clone())
    })
}

/// Server-side decoration configuration.
/// Only colors are user-configurable — everything else is hardcoded.
#[derive(Clone, Debug, PartialEq)]
pub struct DecorationConfig {
    pub bg_color: [u8; 4],
    pub fg_color: [u8; 4],
}

impl Default for DecorationConfig {
    fn default() -> Self {
        Self {
            bg_color: [0x30, 0x30, 0x30, 0xFF],
            fg_color: [0xFF, 0xFF, 0xFF, 0xFF],
        }
    }
}

impl DecorationConfig {
    pub const TITLE_BAR_HEIGHT: i32 = 25;
    pub const SHADOW_RADIUS: f32 = 14.0;
    pub const SHADOW_COLOR: [u8; 4] = [0x00, 0x00, 0x00, 0x20];
    pub const RESIZE_BORDER_WIDTH: i32 = 8;
    pub const CORNER_RADIUS: i32 = 8;
}

/// Built-in dot grid shader — used when no shader_path or tile_path is configured.
pub const DEFAULT_SHADER: &str = include_str!("../../assets/shaders/dot_grid.glsl");

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundConfig {
    /// Path to a GLSL fragment shader. If set, shader is compiled and rendered fullscreen.
    pub shader_path: Option<String>,
    /// Path to a tile image (PNG/JPG). If set, image is tiled across the canvas.
    pub tile_path: Option<String>,
}
