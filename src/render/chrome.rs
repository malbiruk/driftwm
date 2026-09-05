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

#[cfg(test)]
mod tests {
    use super::DrawnChrome;

    fn configured() -> DrawnChrome {
        DrawnChrome {
            ssd_bar: true,
            border_width: 5,
            corner_radius: 8,
            shadow: true,
        }
    }

    #[test]
    fn fullscreen_sheds_all_chrome_even_while_covering() {
        assert_eq!(configured().drawn(true, true), DrawnChrome::NONE);
    }

    #[test]
    fn fullscreen_sheds_all_chrome_while_not_covering() {
        assert_eq!(configured().drawn(true, false), DrawnChrome::NONE);
    }

    #[test]
    fn covering_the_usable_area_squares_corners_and_drops_shadow_but_keeps_bar_and_border() {
        let drawn = configured().drawn(false, true);
        assert_eq!(drawn.corner_radius, 0);
        assert!(!drawn.shadow);
        assert_eq!(drawn.ssd_bar, configured().ssd_bar);
        assert_eq!(drawn.border_width, configured().border_width);
    }

    #[test]
    fn neither_fullscreen_nor_covering_is_the_identity() {
        assert_eq!(configured().drawn(false, false), configured());
    }
}
