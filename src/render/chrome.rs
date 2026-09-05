//! Which chrome a window's picture actually wears, as opposed to the chrome its
//! config asks for.
//!
//! Both compose passes look the configured values up the same way and then have
//! to gate them identically; keeping the two gates — fullscreen, and covering
//! the output's usable area — in one pure function is what stops them drifting.

/// The chrome the compositor draws around one window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrawnChrome {
    pub ssd_bar: bool,
    pub border_width: i32,
    pub corner_radius: i32,
    pub shadow: bool,
}

impl DrawnChrome {
    pub(crate) const NONE: Self = Self {
        ssd_bar: false,
        border_width: 0,
        corner_radius: 0,
        shadow: false,
    };

    /// A fullscreen window sheds all of it; one covering the usable area keeps
    /// its bar and border but draws square and without shadow.
    pub(crate) fn drawn(self, is_fullscreen: bool, covers_usable_area: bool) -> Self {
        if is_fullscreen {
            return Self::NONE;
        }
        if covers_usable_area {
            return Self {
                corner_radius: 0,
                shadow: false,
                ..self
            };
        }
        self
    }
}
