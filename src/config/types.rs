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

/// Cycle relative to the active keyboard layout, or jump to a keymap index.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutSwitch {
    Next,
    Prev,
    Index(usize),
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

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Exec(String),
    ExecTerminal,
    ExecLauncher,
    Spawn(String),
    CloseWindow,
    SuspendWindow,
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
    FillWindow,
    SendToOutput(Direction),
    SendCursorToOutput(Direction),
    FocusCenter,
    TogglePinToScreen,
    SwitchLayout(LayoutSwitch),
    ReloadConfig,
    ToggleCursorPan,
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

    /// Actions the fullscreen guard lets run in place — everything else
    /// exits fullscreen before executing.
    pub fn runs_during_fullscreen(&self) -> bool {
        matches!(
            self,
            Action::ToggleFullscreen
                | Action::Spawn(_)
                | Action::ReloadConfig
                | Action::SwitchLayout(_)
                | Action::ToggleCursorPan
                | Action::SendToOutput(_)
        )
    }

    /// Whether dispatching this during fullscreen ends it — either the guard
    /// exits first, or the action itself does (ToggleFullscreen).
    pub fn ends_fullscreen(&self) -> bool {
        !self.runs_during_fullscreen() || matches!(self, Action::ToggleFullscreen)
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

    pub fn from_state(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
        }
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::EMPTY
    }

    /// Accumulates the high-water mark of a held chord for tap-modifier bindings.
    pub fn union(&self, other: &Self) -> Self {
        Self {
            ctrl: self.ctrl || other.ctrl,
            alt: self.alt || other.alt,
            shift: self.shift || other.shift,
            logo: self.logo || other.logo,
        }
    }

    /// True if every modifier set in `self` is currently held in `state`. The
    /// Alt-Tab cycle commits once its hold modifier is no longer all held.
    pub fn all_held(&self, state: &ModifiersState) -> bool {
        (!self.ctrl || state.ctrl)
            && (!self.alt || state.alt)
            && (!self.shift || state.shift)
            && (!self.logo || state.logo)
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
    /// One discrete wheel notch away from the user. Unlike WheelScroll
    /// (continuous pan/zoom), notch triggers can run any Action.
    WheelUp,
    /// One discrete wheel notch toward the user.
    WheelDown,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MouseBinding {
    pub modifiers: Modifiers,
    pub trigger: MouseTrigger,
}

#[derive(Clone, Debug, PartialEq)]
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
    // Touch-only triggers (never produced by the trackpad gesture parser).
    Tap { fingers: u32 },
    Doubletap { fingers: u32 },
    HoldSwipe { fingers: u32 },
    DoubletapHoldSwipe { fingers: u32 },
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
    /// Same as `MoveWindow` plus cluster drag: every snapped neighbor moves
    /// with the grabbed window. Opt-in via explicit binding.
    MoveSnappedWindows,
    ResizeWindow,
    /// Same as `ResizeWindow` plus cluster propagation: delta applies to the
    /// focused window's snap-cluster neighbors. Opt-in via explicit binding.
    ResizeWindowSnapped,
}

/// Actions for threshold gesture triggers (fire once after accumulation).
#[derive(Clone, Debug, PartialEq)]
pub enum ThresholdAction {
    CenterNearest,
    Fixed(Action),
}

impl ThresholdAction {
    pub fn ends_fullscreen(&self) -> bool {
        match self {
            ThresholdAction::CenterNearest => true, // fires Action::CenterNearest
            ThresholdAction::Fixed(a) => a.ends_fullscreen(),
        }
    }
}

/// Resolved at parse time from trigger + action combination.
#[derive(Clone, Debug, PartialEq)]
pub enum GestureConfigEntry {
    Continuous(ContinuousAction),
    Threshold(ThresholdAction),
}

// ── Context bindings container ───────────────────────────────────────

#[derive(Debug, PartialEq)]
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
pub struct TouchSettings {
    pub enable: bool,
    /// Pin touch input to the output with this connector name. `None` =
    /// auto-detect the touchscreen's output. Only needed to disambiguate
    /// multiple identically-sized touchscreens.
    pub map_to_output: Option<String>,
}

impl Default for TouchSettings {
    fn default() -> Self {
        Self {
            enable: true,
            map_to_output: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MouseDeviceSettings {
    pub accel_speed: f64,
    pub accel_profile: AccelProfile,
    pub natural_scroll: bool,
    pub left_handed: bool,
}

impl Default for MouseDeviceSettings {
    fn default() -> Self {
        Self {
            accel_speed: 0.0,
            accel_profile: AccelProfile::Flat,
            natural_scroll: false,
            left_handed: false,
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
    Minimal,
    /// SSD: nothing at all — bare client surface, no shadow, no corner clip.
    None,
}

/// Font weight for SSD title bar text. The renderer asks fontconfig for the
/// nearest weight the chosen family actually ships.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FontWeight {
    Thin,
    ExtraLight,
    Light,
    Normal,
    #[default]
    Medium,
    SemiBold,
    Bold,
    ExtraBold,
    Black,
}

/// Horizontal placement of SSD title bar text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TitleAlign {
    Left,
    #[default]
    Center,
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

// `regex::Regex` isn't `PartialEq`; compare by source so `Pattern` (hence
// `WindowRule`/`Config`) can derive equality.
impl PartialEq for Pattern {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Pattern::Glob(a), Pattern::Glob(b)) => a == b,
            (Pattern::Regex(a), Pattern::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
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
#[derive(Clone, Debug, Default, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
    pub fullscreen: Option<bool>,
    /// Widget windows are pinned (immovable), excluded from navigation/alt-tab,
    /// and always stacked below normal windows.
    pub widget: bool,
    /// Pin the window to one output's screen space (ignores pan/zoom, renders
    /// above normal windows). When set, `position` is output-relative
    /// (center, Y-up). Combine with `widget = true` to make it immovable.
    pub pinned_to_screen: bool,
    /// Override the global `suspend_on_close` for matched windows. `None`
    /// inherits the global setting.
    pub suspend_on_close: Option<bool>,
    /// `None` means "inherit `[decorations] default_mode`". Explicit
    /// `decoration = "client"` resolves to `Some(Client)` and overrides default.
    pub decoration: Option<DecorationMode>,
    pub blur: bool,
    pub opacity: Option<f64>,
    pub pass_keys: PassKeys,
    /// Per-window border overrides. Ignored for `decoration = "none"` —
    /// use `minimal` mode for a titlebar-less window that still wants a
    /// border. `None` means inherit the global value.
    pub border_width: Option<i32>,
    pub border_color: Option<[u8; 4]>,
    pub border_color_focused: Option<[u8; 4]>,
    /// Per-window corner radius override. Ignored for `decoration = "none"`.
    /// `None` means inherit the global value.
    pub corner_radius: Option<i32>,
    /// Per-window shadow toggle. Ignored for `decoration = "none"`. `None`
    /// means inherit `[decorations] shadow`.
    pub shadow: Option<bool>,
    /// Output name (e.g. `"DP-1"`) this window should fullscreen onto. Overrides
    /// the client-requested output; `None` defers to the client's request, then
    /// the active output.
    pub output: Option<String>,
    /// Stacking order among layer-shell surfaces sharing the same wlr-layer
    /// (the protocol has no z-index within a layer, so map order decides by
    /// default). Higher stacks on top; ties keep map order. Ignored for
    /// regular windows.
    pub layer_order: Option<i32>,
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
#[derive(Clone, Debug, Default)]
pub struct AppliedWindowRule {
    pub fullscreen: Option<bool>,
    pub widget: bool,
    pub pinned_to_screen: bool,
    pub suspend_on_close: Option<bool>,
    pub decoration: Option<DecorationMode>,
    pub blur: bool,
    pub opacity: Option<f64>,
    pub pass_keys: PassKeys,
    /// Explicit window position requested by the matching rule(s).
    pub position: Option<(i32, i32)>,
    /// Explicit window size requested by the matching rule(s).
    pub size: Option<(i32, i32)>,
    pub border_width: Option<i32>,
    pub border_color: Option<[u8; 4]>,
    pub border_color_focused: Option<[u8; 4]>,
    pub corner_radius: Option<i32>,
    pub shadow: Option<bool>,
    pub output: Option<String>,
    pub layer_order: Option<i32>,
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
        if rule.pinned_to_screen {
            self.pinned_to_screen = true;
        }
        if let Some(soc) = rule.suspend_on_close {
            self.suspend_on_close = Some(soc);
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
        if let Some(fs) = rule.fullscreen {
            self.fullscreen = Some(fs);
        }
        if let Some(bw) = rule.border_width {
            self.border_width = Some(bw);
        }
        if let Some(bc) = rule.border_color {
            self.border_color = Some(bc);
        }
        if let Some(bcf) = rule.border_color_focused {
            self.border_color_focused = Some(bcf);
        }
        if let Some(cr) = rule.corner_radius {
            self.corner_radius = Some(cr);
        }
        if let Some(sh) = rule.shadow {
            self.shadow = Some(sh);
        }
        if rule.output.is_some() {
            self.output = rule.output.clone();
        }
        if let Some(lo) = rule.layer_order {
            self.layer_order = Some(lo);
        }
    }
}

impl From<&WindowRule> for AppliedWindowRule {
    fn from(rule: &WindowRule) -> Self {
        Self {
            widget: rule.widget,
            pinned_to_screen: rule.pinned_to_screen,
            suspend_on_close: rule.suspend_on_close,
            decoration: rule.decoration.clone(),
            blur: rule.blur,
            opacity: rule.opacity,
            pass_keys: rule.pass_keys.clone(),
            position: rule.position,
            size: rule.size,
            fullscreen: rule.fullscreen,
            border_width: rule.border_width,
            border_color: rule.border_color,
            border_color_focused: rule.border_color_focused,
            corner_radius: rule.corner_radius,
            shadow: rule.shadow,
            output: rule.output.clone(),
            layer_order: rule.layer_order,
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
    pub disable_hardware_cursor: bool,
    /// Max frames per second delivered to continuous screen-capture clients
    /// (screen recorders / casts). 0 = unlimited. Caps the extra full-scene
    /// re-composite a capture forces, which otherwise rides the compositor's
    /// render rate and competes with a fullscreen client behind it.
    pub max_capture_fps: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EffectsConfig {
    pub blur_radius: u32,
    pub blur_strength: f64,
    /// Refresh rate of the shared animated-background blur; `0` disables the
    /// live refresh (the frost freezes over an animated wallpaper). Animated
    /// wallpapers evolve slowly; re-blurring at a fraction of the output
    /// rate looks continuous through frosted glass at a fraction of the cost.
    pub animate_blur_fps: u32,
}

impl Default for EffectsConfig {
    fn default() -> Self {
        Self {
            blur_radius: 2,
            blur_strength: 1.1,
            animate_blur_fps: 20,
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
#[derive(Clone, Debug, PartialEq)]
pub struct DecorationConfig {
    pub bg_color: [u8; 4],
    pub fg_color: [u8; 4],
    pub corner_radius: i32,
    /// Default decoration mode for windows without a matching rule.
    pub default_mode: DecorationMode,
    /// Border width in pixels. `0` disables the border. Applies to `client`,
    /// `server`, and `minimal` modes; `none` always renders without a border.
    pub border_width: i32,
    pub border_color: [u8; 4],
    pub border_color_focused: [u8; 4],
    /// Global drop-shadow toggle. Per-window `shadow` rules override this.
    pub shadow: bool,
    /// SSD title bar height in logical pixels.
    pub title_bar_height: i32,
    /// Font family for the SSD title text (resolved via fontconfig).
    pub font: String,
    /// Title text size in points (96 dpi reference, like GTK/pango).
    pub font_size: u32,
    pub font_weight: FontWeight,
    pub title_align: TitleAlign,
}

impl Default for DecorationConfig {
    fn default() -> Self {
        Self {
            bg_color: [0x30, 0x30, 0x30, 0xFF],
            fg_color: [0xFF, 0xFF, 0xFF, 0xFF],
            corner_radius: 10,
            default_mode: DecorationMode::Client,
            border_width: 0,
            border_color: [0x30, 0x30, 0x30, 0xFF],
            border_color_focused: [0x30, 0x30, 0x30, 0xFF],
            shadow: true,
            title_bar_height: 25,
            font: "Adwaita Sans".to_string(),
            font_size: 11,
            font_weight: FontWeight::Medium,
            title_align: TitleAlign::Center,
        }
    }
}

/// Effective border width for a window. `decoration = "none"` hard-vetoes the
/// border — use `minimal` if you want a borderless titlebar but still want to
/// opt into a per-window border. Otherwise per-window rule wins, then global.
pub fn effective_border_width(
    applied: Option<&AppliedWindowRule>,
    mode: &DecorationMode,
    decorations: &DecorationConfig,
) -> i32 {
    if matches!(mode, DecorationMode::None) {
        return 0;
    }
    if let Some(bw) = applied.and_then(|r| r.border_width) {
        return bw;
    }
    decorations.border_width
}

/// Effective border color: per-window rule wins, else fall back to global.
/// Mode-independent because color is only consulted when `border_width > 0`.
pub fn effective_border_color(
    applied: Option<&AppliedWindowRule>,
    decorations: &DecorationConfig,
) -> [u8; 4] {
    applied
        .and_then(|r| r.border_color)
        .unwrap_or(decorations.border_color)
}

/// Effective focused-border color: per-window rule wins, else fall back to global.
pub fn effective_border_color_focused(
    applied: Option<&AppliedWindowRule>,
    decorations: &DecorationConfig,
) -> [u8; 4] {
    applied
        .and_then(|r| r.border_color_focused)
        .unwrap_or(decorations.border_color_focused)
}

/// Effective corner radius for a window. `decoration = "none"` hard-vetoes
/// corner clipping (the client surface is passed through as-is). Otherwise
/// per-window rule wins, then global.
pub fn effective_corner_radius(
    applied: Option<&AppliedWindowRule>,
    mode: &DecorationMode,
    decorations: &DecorationConfig,
) -> i32 {
    if matches!(mode, DecorationMode::None) {
        return 0;
    }
    if let Some(cr) = applied.and_then(|r| r.corner_radius) {
        return cr;
    }
    decorations.corner_radius
}

/// Whether to render the compositor shadow for a window. `decoration = "none"`
/// hard-vetoes the shadow. Otherwise per-window rule wins, then global.
pub fn effective_shadow_enabled(
    applied: Option<&AppliedWindowRule>,
    mode: &DecorationMode,
    decorations: &DecorationConfig,
) -> bool {
    if matches!(mode, DecorationMode::None) {
        return false;
    }
    if let Some(s) = applied.and_then(|r| r.shadow) {
        return s;
    }
    decorations.shadow
}

impl DecorationConfig {
    pub const SHADOW_RADIUS: f32 = 14.0;
    pub const SHADOW_COLOR: [u8; 4] = [0x00, 0x00, 0x00, 0x66];
    pub const RESIZE_BORDER_WIDTH: i32 = 8;
}

/// Settings for drawing outlines of other monitors' viewports.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
pub struct OutputConfig {
    pub name: String,
    pub scale: Option<f64>,
    pub transform: Option<Transform>,
    pub position: OutputPosition,
    pub mode: OutputMode,
    pub hot_corners: HotCorners,
}

/// Which corner of the screen to bind a hot-corner action to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HotCorner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl HotCorner {
    /// True when `(x, y)` in output-local screen coords is within `threshold`
    /// pixels of this corner.
    pub fn contains(&self, x: f64, y: f64, out_w: f64, out_h: f64, threshold: f64) -> bool {
        let on_left = x < threshold;
        let on_right = x > out_w - threshold;
        let on_top = y < threshold;
        let on_bottom = y > out_h - threshold;
        match self {
            Self::TopLeft => on_left && on_top,
            Self::TopRight => on_right && on_top,
            Self::BottomLeft => on_left && on_bottom,
            Self::BottomRight => on_right && on_bottom,
        }
    }
}

/// Per-output hot-corner bindings. Empty map = no corners configured.
#[derive(Clone, Debug, PartialEq)]
pub struct HotCorners {
    /// Action to fire when the cursor enters this corner. `None` disables the corner.
    pub bindings: HashMap<HotCorner, Action>,
    /// Activation zone size in logical pixels. Defaults to 4.
    pub threshold: f64,
    pub disable_when_fullscreen: bool,
    pub disable_while_dragging: bool,
}

impl Default for HotCorners {
    fn default() -> Self {
        Self {
            bindings: HashMap::new(),
            threshold: Self::DEFAULT_THRESHOLD,
            disable_when_fullscreen: true,
            disable_while_dragging: true,
        }
    }
}

impl HotCorners {
    /// Default activation zone size in logical pixels. Tuned for trackpad
    /// precision: small enough not to eat normal cursor travel, large enough
    /// to catch a deliberate flick to a corner.
    pub const DEFAULT_THRESHOLD: f64 = 4.0;
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
    /// Highest resolution (w*h), then highest refresh.
    Max,
}

/// Built-in dot grid shader — used when no background source is configured.
pub const DEFAULT_SHADER: &str = include_str!("../shaders/dot_grid.glsl");

/// Background source. The three image-bearing variants are intentionally
/// orthogonal to "animated vs not" — animation is detected from the shader
/// source (`u_time`) or, in future, from file extensions for tile/wallpaper.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum BackgroundKind {
    /// Procedural GLSL fullscreen shader. `texture` is an optional image the
    /// shader samples via the built-in `tex` sampler (single-texture support).
    Shader {
        path: String,
        texture: Option<String>,
    },
    /// Texture tiled across canvas via `tile_bg.glsl` (scrolls with camera).
    Tile(String),
    /// Single image filling the viewport (fixed; does not scroll/zoom).
    Wallpaper(String),
    /// No built-in background — render nothing so a wlr-layer-shell Background
    /// surface (swaybg, mpvpaper, …) or the clear color is the wallpaper.
    None,
    /// Built-in dot grid shader.
    #[default]
    Default,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundConfig {
    pub kind: BackgroundKind,
    /// Mirror-fold a `Tile` image so non-seamless edges meet a reflection,
    /// hiding tile seams. No-op for non-tile backgrounds.
    pub mirror_tile: bool,
    /// Bake a static `u_camera`-only shader to chunked GPU textures and pan
    /// those, instead of recomputing the fragment shader every frame. No-op for
    /// non-shader backgrounds and for shaders using `u_time`/`u_zoom`.
    pub cache_shader: bool,
    /// Honor a shader background's output alpha so transparent pixels reveal
    /// whatever sits below (e.g. a wlr-layer-shell Background surface). Drops
    /// the fullscreen-opaque fast path and forces shaders onto the live render
    /// path, so `cache_shader` has no effect while this is on. No-op for image
    /// backgrounds, which detect transparency from the image's own alpha.
    pub transparent_shader: bool,
    /// Memory ceiling (MB) for the background chunk cache — covers both the
    /// shader-bake cache (`cache_shader`) and gigapixel-TIFF wallpapers. The
    /// cache LRU-evicts to stay under this; lower it on low-memory machines.
    pub cache_budget_mb: u32,
    /// Frame-rate cap for animated (`u_time`) background shaders. 0 = every
    /// output frame (default). Slow-moving shaders look identical well below
    /// the refresh rate, and between animation ticks the compositor reuses the
    /// composited result instead of re-evaluating the shader.
    pub animate_fps: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_during_fullscreen_allowlist() {
        assert!(Action::ToggleFullscreen.runs_during_fullscreen());
        assert!(Action::Spawn("foo".into()).runs_during_fullscreen());
        assert!(Action::ReloadConfig.runs_during_fullscreen());
        assert!(Action::SwitchLayout(LayoutSwitch::Next).runs_during_fullscreen());
        assert!(Action::ToggleCursorPan.runs_during_fullscreen());
        assert!(Action::SendToOutput(Direction::Right).runs_during_fullscreen());
        assert!(!Action::CloseWindow.runs_during_fullscreen());
        assert!(!Action::ZoomIn.runs_during_fullscreen());
        assert!(!Action::CenterNearest(Direction::Up).runs_during_fullscreen());
    }

    #[test]
    fn action_ends_fullscreen() {
        // ends it via its own handler despite being allowlisted
        assert!(Action::ToggleFullscreen.ends_fullscreen());
        assert!(!Action::Spawn("foo".into()).ends_fullscreen());
        assert!(Action::CloseWindow.ends_fullscreen());
    }

    #[test]
    fn threshold_action_ends_fullscreen() {
        assert!(ThresholdAction::CenterNearest.ends_fullscreen());
        assert!(!ThresholdAction::Fixed(Action::Spawn("foo".into())).ends_fullscreen());
        assert!(ThresholdAction::Fixed(Action::CloseWindow).ends_fullscreen());
    }
}
