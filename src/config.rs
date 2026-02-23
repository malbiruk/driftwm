use std::collections::HashMap;

use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

#[derive(Clone, Debug)]
pub enum Direction {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug)]
pub enum Action {
    SpawnCommand(String),
    CloseWindow,
    NudgeWindow(Direction),
    PanViewport(Direction),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

/// Which physical key acts as the window-manager modifier.
/// Alt for dev (nested winit); Super for production (standalone on TTY).
#[derive(Clone, Copy, Debug)]
pub enum ModKey {
    Alt,
    Super,
}

impl ModKey {
    /// Base modifier pattern with only the WM mod key set.
    fn base(self) -> Modifiers {
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

impl Modifiers {
    const EMPTY: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
    };

    fn from_state(state: &ModifiersState) -> Self {
        Self {
            ctrl: state.ctrl,
            alt: state.alt,
            shift: state.shift,
            logo: state.logo,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub modifiers: Modifiers,
    pub sym: Keysym,
}

#[derive(Clone, Debug)]
pub struct BackgroundConfig {
    /// Clear color / solid fallback (normalized RGBA). Shows through when no shader or tile.
    pub bg_color: [f32; 4],
    /// Path to a GLSL fragment shader. If set, shader is compiled and rendered fullscreen.
    pub shader_path: Option<String>,
    /// Path to a tile image (PNG/JPG). If set, image is tiled across the canvas.
    pub tile_path: Option<String>,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            bg_color: [0.0, 0.0, 0.0, 1.0],
            shader_path: Some("assets/shaders/pink_cloud.glsl".to_string()),
            tile_path: None,
        }
    }
}

pub struct Config {
    pub mod_key: ModKey,
    /// Multiplier for scroll deltas. Higher = faster initial scroll. 1.0 = raw trackpad.
    pub scroll_speed: f64,
    /// Scroll momentum decay factor per frame. 0.92 = snappy, 0.96 = floaty.
    pub friction: f64,
    pub background: BackgroundConfig,
    bindings: HashMap<KeyCombo, Action>,
}

impl Config {
    pub fn lookup(&self, modifiers: &ModifiersState, sym: Keysym) -> Option<&Action> {
        let combo = KeyCombo {
            modifiers: Modifiers::from_state(modifiers),
            sym,
        };
        self.bindings.get(&combo)
    }
}

impl Default for Config {
    fn default() -> Self {
        let mod_key = ModKey::Alt;
        let terminal = detect_terminal();
        tracing::info!("Terminal command: {terminal}");

        let m = mod_key.base();
        let m_shift = Modifiers {
            shift: true,
            ..m.clone()
        };
        let m_ctrl = Modifiers {
            ctrl: true,
            ..m.clone()
        };

        let bindings = HashMap::from([
            (
                KeyCombo {
                    modifiers: m.clone(),
                    sym: Keysym::from(keysyms::KEY_Return),
                },
                Action::SpawnCommand(terminal),
            ),
            (
                KeyCombo {
                    modifiers: m,
                    sym: Keysym::from(keysyms::KEY_q),
                },
                Action::CloseWindow,
            ),
            // Window nudge: Mod+Shift+Arrow
            (
                KeyCombo {
                    modifiers: m_shift.clone(),
                    sym: Keysym::from(keysyms::KEY_Up),
                },
                Action::NudgeWindow(Direction::Up),
            ),
            (
                KeyCombo {
                    modifiers: m_shift.clone(),
                    sym: Keysym::from(keysyms::KEY_Down),
                },
                Action::NudgeWindow(Direction::Down),
            ),
            (
                KeyCombo {
                    modifiers: m_shift.clone(),
                    sym: Keysym::from(keysyms::KEY_Left),
                },
                Action::NudgeWindow(Direction::Left),
            ),
            (
                KeyCombo {
                    modifiers: m_shift,
                    sym: Keysym::from(keysyms::KEY_Right),
                },
                Action::NudgeWindow(Direction::Right),
            ),
            // Viewport panning: Mod+Ctrl+Arrow
            (
                KeyCombo {
                    modifiers: m_ctrl.clone(),
                    sym: Keysym::from(keysyms::KEY_Up),
                },
                Action::PanViewport(Direction::Up),
            ),
            (
                KeyCombo {
                    modifiers: m_ctrl.clone(),
                    sym: Keysym::from(keysyms::KEY_Down),
                },
                Action::PanViewport(Direction::Down),
            ),
            (
                KeyCombo {
                    modifiers: m_ctrl.clone(),
                    sym: Keysym::from(keysyms::KEY_Left),
                },
                Action::PanViewport(Direction::Left),
            ),
            (
                KeyCombo {
                    modifiers: m_ctrl,
                    sym: Keysym::from(keysyms::KEY_Right),
                },
                Action::PanViewport(Direction::Right),
            ),
        ]);

        Self {
            mod_key,
            scroll_speed: 1.5,
            friction: 0.96,
            background: BackgroundConfig::default(),
            bindings,
        }
    }
}

fn detect_terminal() -> String {
    if let Ok(term) = std::env::var("TERMINAL")
        && !term.is_empty()
    {
        return term;
    }
    for cmd in ["foot", "alacritty", "ptyxis", "kitty", "wezterm"] {
        if std::process::Command::new("which")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
        {
            return cmd.to_string();
        }
    }
    "foot".to_string()
}
