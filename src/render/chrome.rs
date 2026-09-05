//! Which chrome a window's picture actually wears, as opposed to the chrome its
//! config asks for.
//!
//! Both compose passes look the configured values up the same way and then have
//! to gate them identically; keeping the two gates — fullscreen, and how the
//! window's frame covers the output's usable area — in one pure function is what
//! stops them drifting.

use driftwm::canvas::Coverage;

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

    /// A fullscreen window sheds all of it. One flush with the usable area keeps
    /// its bar and border but draws square and without shadow; one hanging past
    /// that edge loses the border too, which would otherwise be drawn under a
    /// panel or off the output instead of around the window.
    pub(crate) fn drawn(self, is_fullscreen: bool, coverage: Coverage) -> Self {
        if is_fullscreen {
            return Self::NONE;
        }
        match coverage {
            Coverage::None => self,
            Coverage::Exact => Self {
                corner_radius: 0,
                shadow: false,
                ..self
            },
            Coverage::Overhang => Self {
                border_width: 0,
                corner_radius: 0,
                shadow: false,
                ..self
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DrawnChrome;
    use driftwm::canvas::Coverage;

    fn configured() -> DrawnChrome {
        DrawnChrome {
            ssd_bar: true,
            border_width: 5,
            corner_radius: 8,
            shadow: true,
        }
    }

    #[test]
    fn fullscreen_sheds_all_chrome_at_every_coverage() {
        for coverage in [Coverage::None, Coverage::Exact, Coverage::Overhang] {
            assert_eq!(configured().drawn(true, coverage), DrawnChrome::NONE);
        }
    }

    #[test]
    fn an_exact_cover_squares_corners_and_drops_shadow_but_keeps_bar_and_border() {
        let drawn = configured().drawn(false, Coverage::Exact);
        assert_eq!(drawn.corner_radius, 0);
        assert!(!drawn.shadow);
        assert_eq!(drawn.ssd_bar, configured().ssd_bar);
        assert_eq!(drawn.border_width, configured().border_width);
    }

    #[test]
    fn an_overhanging_cover_drops_the_border_too_and_keeps_the_bar() {
        let drawn = configured().drawn(false, Coverage::Overhang);
        assert_eq!(drawn.border_width, 0);
        assert_eq!(drawn.corner_radius, 0);
        assert!(!drawn.shadow);
        assert_eq!(drawn.ssd_bar, configured().ssd_bar);
    }

    #[test]
    fn no_coverage_is_the_identity() {
        assert_eq!(configured().drawn(false, Coverage::None), configured());
    }
}
