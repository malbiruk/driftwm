use std::collections::HashMap;
use std::f64::consts::FRAC_1_SQRT_2;
use std::hash::Hash;

use smithay::input::keyboard::{Keysym, ModifiersState};
use smithay::utils::Transform;

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
    ZoomToFitSnapped,
    ToggleFullscreen,
    FitWindow,
    FitWindowSnapped,
    SendToOutput(Direction),
    FocusCenter,
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
    pub const EMPTY: Self = Self {
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

/// How a new window is placed on the canvas when no window rule positions it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WindowPlacement {
    /// Center on the active output's viewport (and trigger navigate-to).
    #[default]
    Center,
    /// Center on the cursor, clamped to the active output's usable canvas rect.
    /// Skips the auto-navigate so the camera stays where the user pointed.
    Cursor,
    /// Snap-place adjacent to the focused window's cluster: try focused's
    /// edges (CW from viewport-nearest), then BFS to neighbors. Falls back
    /// to `Center` when no focused window or no valid placement was found.
    Auto,
    /// Always place next to an existing visible window — never on top.
    /// Scans all windows for a free adjacent spot, tries focus history first,
    /// then every window on the canvas. Falls back to center only when the
    /// canvas has no windows at all.
    Tiling,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BindingContext {
    OnWindow,
    OnCanvas,
    Anywhere,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MouseTrigger {
    Button(u32),
    TrackpadScroll,
    WheelScroll,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MouseBinding {
    pub modifiers: Modifiers,
    pub trigger: MouseTrigger,
}

#[derive(Clone, Debug)]
pub enum MouseAction {
    MoveWindow,
    /// Drag every window connected to the focused one via snap adjacency
    /// (edge-flush with `snap_gap`). The cluster is computed on demand at
    /// drag start; use a separate binding from `MoveWindow` so that grabbing
    /// a window never implicitly drags neighbors.
    MoveSnappedWindows,
    ResizeWindow,
    /// Resize the focused window and propagate the delta to every snapped
    /// neighbor in its cluster. Same opt-in shape as `MoveSnappedWindows`:
    /// grabbing a window never implicitly resizes neighbors — the user
    /// must bind this action explicitly.
    ResizeWindowSnapped,
    PanViewport,
    Zoom,
    CenterNearest,
    Action(Action),
}

// ── Gesture types ────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum GestureTrigger {
    Swipe { fingers: u32 },
    SwipeUp { fingers: u32 },
    SwipeDown { fingers: u32 },
    SwipeLeft { fingers: u32 },
    SwipeRight { fingers: u32 },
    DoubletapSwipe { fingers: u32 },
    Pinch { fingers: u32 },
    PinchIn { fingers: u32 },
    PinchOut { fingers: u32 },
    Hold { fingers: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GestureBinding {
    pub modifiers: Modifiers,
    pub trigger: GestureTrigger,
}

/// Actions for continuous gesture/mouse triggers (per-frame updates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContinuousAction {
    PanViewport,
    Zoom,
    MoveWindow,
    ResizeWindow,
    /// Same as `ResizeWindow` plus cluster propagation: delta applies to the
    /// focused window's snap-cluster neighbors. Opt-in via explicit binding.
    ResizeWindowSnapped,
}

/// Actions for threshold gesture triggers (fire once after accumulation).
#[derive(Clone, Debug)]
pub enum ThresholdAction {
    CenterNearest,
    Fixed(Action),
}

/// Resolved at parse time from trigger + action combination.
#[derive(Clone, Debug)]
pub enum GestureConfigEntry {
    Continuous(ContinuousAction),
    Threshold(ThresholdAction),
}

// ── Context bindings container ───────────────────────────────────────

pub struct ContextBindings<K: Eq + Hash, V> {
    pub on_window: HashMap<K, V>,
    pub on_canvas: HashMap<K, V>,
    pub anywhere: HashMap<K, V>,
}

impl<K: Eq + Hash, V> ContextBindings<K, V> {
    pub fn empty() -> Self {
        Self {
            on_window: HashMap::new(),
            on_canvas: HashMap::new(),
            anywhere: HashMap::new(),
        }
    }

    pub fn lookup(&self, key: &K, context: BindingContext) -> Option<&V> {
        let specific = match context {
            BindingContext::OnWindow => &self.on_window,
            BindingContext::OnCanvas => &self.on_canvas,
            BindingContext::Anywhere => return self.anywhere.get(key),
        };
        specific.get(key).or_else(|| self.anywhere.get(key))
    }

    fn context_map_mut(&mut self, context: BindingContext) -> &mut HashMap<K, V> {
        match context {
            BindingContext::OnWindow => &mut self.on_window,
            BindingContext::OnCanvas => &mut self.on_canvas,
            BindingContext::Anywhere => &mut self.anywhere,
        }
    }

    pub fn insert(&mut self, context: BindingContext, key: K, value: V) {
        self.context_map_mut(context).insert(key, value);
    }

    pub fn remove(&mut self, context: BindingContext, key: &K) {
        self.context_map_mut(context).remove(key);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelProfile {
    Flat,
    Adaptive,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrackpadSettings {
    pub tap_to_click: bool,
    pub natural_scroll: bool,
    pub tap_and_drag: bool,
    pub accel_speed: f64,
    pub accel_profile: AccelProfile,
    pub click_method: Option<String>,
    pub disable_while_typing: bool,
}

impl Default for TrackpadSettings {
    fn default() -> Self {
        Self {
            tap_to_click: true,
            natural_scroll: true,
            tap_and_drag: true,
            accel_speed: 0.0,
            accel_profile: AccelProfile::Adaptive,
            click_method: None,
            disable_while_typing: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MouseDeviceSettings {
    pub accel_speed: f64,
    pub accel_profile: AccelProfile,
    pub natural_scroll: bool,
}

impl Default for MouseDeviceSettings {
    fn default() -> Self {
        Self {
            accel_speed: 0.0,
            accel_profile: AccelProfile::Flat,
            natural_scroll: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GestureThresholds {
    pub swipe_distance: f64,
    pub pinch_in_scale: f64,
    pub pinch_out_scale: f64,
}

impl Default for GestureThresholds {
    fn default() -> Self {
        Self {
            swipe_distance: 12.0,
            pinch_in_scale: 0.85,
            pinch_out_scale: 1.15,
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

/// Decoration mode for a window. Drives both the xdg-decoration hint we send
/// the client and what the compositor renders on top.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DecorationMode {
    /// CSD: client draws its own decorations. Compositor still draws shadow + corner clip.
    #[default]
    Client,
    /// SSD: compositor draws a title bar with close button, plus shadow + corner clip.
    Server,
    /// SSD: no title bar, but compositor still draws shadow + corner clip.
    Borderless,
    /// SSD: nothing at all — bare client surface, no shadow, no corner clip.
    None,
}

// ── Window-rule pattern matching ────────────────────────────────────

/// A match pattern for a window rule field.
/// - `Glob`: simple wildcard matching (`*` matches any sequence of chars).
/// - `Regex`: full regular expression (wrap in `/…/` in config).
#[derive(Clone, Debug)]
pub enum Pattern {
    Glob(String),
    Regex(regex::Regex),
}

impl Pattern {
    /// Returns true if `value` matches this pattern.
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Pattern::Glob(pat) => glob_matches(pat, value),
            Pattern::Regex(re) => re.is_match(value),
        }
    }
}

/// Simple glob match — `*` matches any sequence of characters (including empty).
/// Multiple `*` wildcards are supported. Case-sensitive.
pub fn glob_matches(pat: &str, val: &str) -> bool {
    let parts: Vec<&str> = pat.split('*').collect();
    if parts.len() == 1 {
        return pat == val;
    }
    let mut remaining = val;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if i == 0 {
            // first segment must be an exact prefix
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == parts.len() - 1 {
            // last segment must be an exact suffix
            return remaining.ends_with(part);
        } else {
            // middle segments: find anywhere in the remaining string
            if let Some(pos) = remaining.find(part) {
                remaining = &remaining[pos + part.len()..];
            } else {
                return false;
            }
        }
    }
    true
}

/// Controls which compositor keybindings are forwarded to the focused window
/// instead of being handled by the compositor.
///
/// - `None`  — default; compositor handles everything.
/// - `All`   — all keys forwarded (game/fullscreen-friendly).
/// - `Only`  — only the listed combos are forwarded; all others stay active.
#[derive(Clone, Debug, Default)]
pub enum PassKeys {
    #[default]
    None,
    All,
    Only(Vec<KeyCombo>),
}

impl PassKeys {
    /// Returns `true` if the given raw key event should be forwarded to the app.
    /// Constructs and normalises a `KeyCombo` internally — no import needed at call sites.
    pub fn allows_raw(&self, modifiers: &ModifiersState, sym: Keysym) -> bool {
        match self {
            PassKeys::None => false,
            PassKeys::All => true,
            PassKeys::Only(combos) => {
                let mut current = KeyCombo {
                    modifiers: Modifiers::from_state(modifiers),
                    sym,
                };
                current.normalize();
                combos.iter().any(|c| c == &current)
            }
        }
    }

    /// Merge `other` into `self`.
    /// `All` is sticky-on; `Only` lists are unioned; `None` is a no-op.
    pub fn merge_from(&mut self, other: &PassKeys) {
        match (&*self, other) {
            (_, PassKeys::None) => {} // other adds nothing
            (PassKeys::All, _) => {}  // already maximally permissive
            (_, PassKeys::All) => *self = PassKeys::All,
            (PassKeys::None, PassKeys::Only(v)) => *self = PassKeys::Only(v.clone()),
            (PassKeys::Only(existing), PassKeys::Only(extra)) => {
                let mut merged = existing.clone();
                for c in extra {
                    if !merged.contains(c) {
                        merged.push(c.clone());
                    }
                }
                *self = PassKeys::Only(merged);
            }
        }
    }
}

/// Parsed window rule from config.
#[derive(Clone, Debug)]
pub struct WindowRule {
    // ── Match criteria (all that are Some must match) ─────────────────
    /// Wayland `app_id`. X11 apps proxied via xwayland-satellite arrive
    /// with `app_id` set from `WM_CLASS` instance (typically lowercase).
    pub app_id: Option<Pattern>,
    /// Window title.
    pub title: Option<Pattern>,
    // ── One-time placement effects ────────────────────────────────────
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
    /// Widget windows are pinned (immovable), excluded from navigation/alt-tab,
    /// and always stacked below normal windows.
    pub widget: bool,
    /// `None` means "inherit `[decorations] default_mode`". Explicit
    /// `decoration = "client"` resolves to `Some(Client)` and overrides default.
    pub decoration: Option<DecorationMode>,
    pub blur: bool,
    pub opacity: Option<f64>,
    pub pass_keys: PassKeys,
}

impl WindowRule {
    /// Returns true if this rule matches all of the supplied window identifiers.
    /// Fields that are `None` are treated as wildcards (match anything).
    pub fn matches(&self, app_id: &str, title: &str) -> bool {
        let app_ok = self.app_id.as_ref().is_none_or(|p| p.matches(app_id));
        let ttl_ok = self.title.as_ref().is_none_or(|p| p.matches(title));
        app_ok && ttl_ok
    }

    /// True if at least one match criterion is set (rules with no criteria are rejected).
    pub fn has_criteria(&self) -> bool {
        self.app_id.is_some() || self.title.is_some()
    }
}

/// Runtime rule state stored in a surface's data_map after matching.
/// Built by merging ALL matching `WindowRule`s in config order
/// (later rules override earlier ones for scalar fields; boolean flags
/// are sticky-on).
#[derive(Clone, Debug)]
pub struct AppliedWindowRule {
    pub widget: bool,
    pub decoration: Option<DecorationMode>,
    pub blur: bool,
    pub opacity: Option<f64>,
    pub pass_keys: PassKeys,
    /// Explicit window position requested by the matching rule(s).
    pub position: Option<(i32, i32)>,
    /// Explicit window size requested by the matching rule(s).
    pub size: Option<(i32, i32)>,
}

impl AppliedWindowRule {
    /// Overlay `rule`'s effects on top of `self`.
    /// Boolean flags (widget, blur) are sticky-on.
    /// `pass_keys`: `All` is sticky-on; `Only` lists are unioned (see `PassKeys::merge_from`).
    /// Scalar fields (decoration, opacity, position, size) use last-wins.
    pub fn merge_from(&mut self, rule: &WindowRule) {
        if rule.widget {
            self.widget = true;
        }
        if rule.blur {
            self.blur = true;
        }
        self.pass_keys.merge_from(&rule.pass_keys);
        if rule.decoration.is_some() {
            self.decoration = rule.decoration.clone();
        }
        if let Some(op) = rule.opacity {
            self.opacity = Some(op);
        }
        if let Some(pos) = rule.position {
            self.position = Some(pos);
        }
        if let Some(sz) = rule.size {
            self.size = Some(sz);
        }
    }
}

impl From<&WindowRule> for AppliedWindowRule {
    fn from(rule: &WindowRule) -> Self {
        Self {
            widget: rule.widget,
            decoration: rule.decoration.clone(),
            blur: rule.blur,
            opacity: rule.opacity,
            pass_keys: rule.pass_keys.clone(),
            position: rule.position,
            size: rule.size,
        }
    }
}

/// Resolve the effective decoration mode for a window: rule-specified mode wins;
/// otherwise fall back to the global `default_mode` from `[decorations]`.
/// Accepts the decoration field directly (works with both `WindowRule` and
/// `AppliedWindowRule`).
pub fn effective_decoration_mode<'a>(
    rule_decoration: Option<&'a DecorationMode>,
    default_mode: &'a DecorationMode,
) -> &'a DecorationMode {
    rule_decoration.unwrap_or(default_mode)
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct BackendConfig {
    pub wait_for_frame_completion: bool,
    pub disable_direct_scanout: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectsConfig {
    pub blur_radius: u32,
    pub blur_strength: f64,
}

impl Default for EffectsConfig {
    fn default() -> Self {
        Self {
            blur_radius: 2,
            blur_strength: 1.1,
        }
    }
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
    pub corner_radius: i32,
    /// Default decoration mode for windows without a matching rule.
    pub default_mode: DecorationMode,
}

impl Default for DecorationConfig {
    fn default() -> Self {
        Self {
            bg_color: [0x30, 0x30, 0x30, 0xFF],
            fg_color: [0xFF, 0xFF, 0xFF, 0xFF],
            corner_radius: 10,
            default_mode: DecorationMode::Client,
        }
    }
}

impl DecorationConfig {
    pub const TITLE_BAR_HEIGHT: i32 = 25;
    pub const SHADOW_RADIUS: f32 = 14.0;
    pub const SHADOW_COLOR: [u8; 4] = [0x00, 0x00, 0x00, 0x66];
    pub const RESIZE_BORDER_WIDTH: i32 = 8;
}

/// Settings for drawing outlines of other monitors' viewports.
#[derive(Clone, Debug)]
pub struct OutputOutlineSettings {
    pub color: [u8; 4],
    pub thickness: i32,
    pub opacity: f64,
}

impl Default for OutputOutlineSettings {
    fn default() -> Self {
        Self {
            color: [0xFF, 0xFF, 0xFF, 0xFF],
            thickness: 1,
            opacity: 0.5,
        }
    }
}

/// Per-output configuration from `[[outputs]]` config sections.
#[derive(Clone, Debug)]
pub struct OutputConfig {
    pub name: String,
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
    pub position: OutputPosition,
    pub mode: OutputMode,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum OutputPosition {
    #[default]
    Auto,
    Fixed(i32, i32),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum OutputMode {
    #[default]
    Preferred,
    /// WxH — pick highest refresh rate.
    Size(i32, i32),
    /// WxH@Hz — approximate match (DRM reports millihertz).
    SizeRefresh(i32, i32, u32),
}

/// Built-in dot grid shader — used when no background source is configured.
pub const DEFAULT_SHADER: &str = include_str!("../shaders/dot_grid.glsl");

/// Background source. The three image-bearing variants are intentionally
/// orthogonal to "animated vs not" — animation is detected from the shader
/// source (`u_time`) or, in future, from file extensions for tile/wallpaper.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum BackgroundKind {
    /// Procedural GLSL fullscreen shader.
    Shader(String),
    /// Texture tiled across canvas via `tile_bg.glsl` (scrolls with camera).
    Tile(String),
    /// Single image filling the viewport (fixed; does not scroll/zoom).
    Wallpaper(String),
    /// Built-in dot grid shader.
    #[default]
    Default,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundConfig {
    pub kind: BackgroundKind,
}
