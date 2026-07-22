use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use driftwm::config::{DecorationConfig, TitleAlign};

/// Per-window SSD decoration state.
pub struct WindowDecoration {
    pub title_bar: MemoryRenderBuffer,
    pub width: i32,
    pub focused: bool,
    pub close_hovered: bool,
    pub scale: i32,
    pub title: String,
    /// Draw the screen-pinned indicator dot near the left edge.
    pub pinned: bool,
    /// Font-load state at last render: flips a textless bar to re-render once
    /// the background font scan lands.
    fonts_ready: bool,
}

/// What the pointer is over in SSD decoration space.
#[derive(Debug, Clone, Copy)]
pub enum DecorationHit {
    TitleBar,
    CloseButton,
    ResizeBorder(xdg_toplevel::ResizeEdge),
    /// Suspended-window body outside the centered label — focus + raise.
    Body,
    /// Suspended-window centered label — relaunch the app.
    Label,
}

/// Key for the SSD decoration + border + shadow caches. A client window keys on
/// its surface id; a suspended window keys on its durable id (no surface). One
/// map serves both so the chrome lifecycle stays single-path.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DecorationKey {
    Surface(smithay::reexports::wayland_server::backend::ObjectId),
    Suspended(crate::state::SuspendedId),
}

impl From<smithay::reexports::wayland_server::backend::ObjectId> for DecorationKey {
    fn from(id: smithay::reexports::wayland_server::backend::ObjectId) -> Self {
        DecorationKey::Surface(id)
    }
}

impl From<crate::state::SuspendedId> for DecorationKey {
    fn from(id: crate::state::SuspendedId) -> Self {
        DecorationKey::Suspended(id)
    }
}

impl WindowDecoration {
    pub fn new(width: i32, focused: bool, config: &DecorationConfig) -> Self {
        // Placeholder scale + empty title; the first-frame `update()` re-renders
        // at the real output scale and window title.
        let scale = 1;
        let title_bar = render_title_bar(width, focused, false, scale, "", false, config);
        Self {
            title_bar,
            width,
            focused,
            close_hovered: false,
            scale,
            title: String::new(),
            pinned: false,
            fonts_ready: driftwm::text::fonts_ready(),
        }
    }

    /// Re-render if width, focus, pinned, scale, or title changed. Returns true if rebuilt.
    pub fn update(
        &mut self,
        width: i32,
        focused: bool,
        pinned: bool,
        scale: i32,
        title: &str,
        config: &DecorationConfig,
    ) -> bool {
        let fonts_ready = driftwm::text::fonts_ready();
        if width == self.width
            && focused == self.focused
            && pinned == self.pinned
            && scale == self.scale
            && title == self.title
            && fonts_ready == self.fonts_ready
        {
            return false;
        }
        self.width = width;
        self.focused = focused;
        self.pinned = pinned;
        self.scale = scale;
        self.fonts_ready = fonts_ready;
        self.title.clear();
        self.title.push_str(title);
        self.title_bar = render_title_bar(
            width,
            focused,
            self.close_hovered,
            scale,
            &self.title,
            self.pinned,
            config,
        );
        true
    }
}

/// Right padding so the close button doesn't sit flush with the title bar edge.
const CLOSE_BTN_RIGHT_PAD: i32 = 8;
/// Gap between the title text and the close button (logical px).
const TITLE_TEXT_GAP: i32 = 6;
/// Points-to-logical-pixels factor (96 dpi reference). `font_size` is
/// configured in points to match GTK/pango font specs.
const PT_TO_PX: f32 = 4.0 / 3.0;
/// Close-button × stroke width as a fraction of the title bar height
/// (~1.25px at the default 25px bar). Scales with bar height and output scale.
const CLOSE_BTN_STROKE: f64 = 0.05;

/// Close button hit area: a square on the right side of the title bar.
pub fn close_button_rect(
    window_loc: Point<i32, Logical>,
    width: i32,
    bar_height: i32,
) -> Rectangle<i32, Logical> {
    let btn_size = bar_height;
    Rectangle::new(
        Point::from((
            window_loc.x + width - btn_size - CLOSE_BTN_RIGHT_PAD,
            window_loc.y - bar_height,
        )),
        Size::from((btn_size, btn_size)),
    )
}

/// Check if a canvas position is within the title bar (excluding close button).
pub fn title_bar_contains(
    pos: Point<f64, Logical>,
    window_loc: Point<i32, Logical>,
    width: i32,
    bar_height: i32,
) -> bool {
    let x = pos.x;
    let y = pos.y;
    let bar_top = window_loc.y as f64 - bar_height as f64;
    let bar_bottom = window_loc.y as f64;
    let bar_left = window_loc.x as f64;
    let bar_right = bar_left + width as f64 - bar_height as f64 - CLOSE_BTN_RIGHT_PAD as f64;
    x >= bar_left && x < bar_right && y >= bar_top && y < bar_bottom
}

/// Check if a canvas position is within the close button.
pub fn close_button_contains(
    pos: Point<f64, Logical>,
    window_loc: Point<i32, Logical>,
    width: i32,
    bar_height: i32,
) -> bool {
    let rect = close_button_rect(window_loc, width, bar_height);
    pos.x >= rect.loc.x as f64
        && pos.x < (rect.loc.x + rect.size.w) as f64
        && pos.y >= rect.loc.y as f64
        && pos.y < (rect.loc.y + rect.size.h) as f64
}

/// Hit-test invisible resize borders around the window + title bar.
/// Returns the resize edge if the position is within the border zone.
pub fn resize_edge_at(
    pos: Point<f64, Logical>,
    window_loc: Point<i32, Logical>,
    window_size: Size<i32, Logical>,
    bar_height: i32,
    border_width: i32,
) -> Option<xdg_toplevel::ResizeEdge> {
    let bw = border_width as f64;
    let left = window_loc.x as f64 - bw;
    let right = (window_loc.x + window_size.w) as f64 + bw;
    let top = (window_loc.y - bar_height) as f64 - bw;
    let bottom = (window_loc.y + window_size.h) as f64 + bw;

    if pos.x < left || pos.x >= right || pos.y < top || pos.y >= bottom {
        return None;
    }

    let inner_left = window_loc.x as f64;
    let inner_right = (window_loc.x + window_size.w) as f64;
    let inner_top = (window_loc.y - bar_height) as f64;
    let inner_bottom = (window_loc.y + window_size.h) as f64;

    // Already inside the window+titlebar area — not a resize border
    if pos.x >= inner_left && pos.x < inner_right && pos.y >= inner_top && pos.y < inner_bottom {
        return None;
    }

    let in_left = pos.x < inner_left;
    let in_right = pos.x >= inner_right;
    let in_top = pos.y < inner_top;
    let in_bottom = pos.y >= inner_bottom;

    Some(match (in_left, in_right, in_top, in_bottom) {
        (true, _, true, _) => xdg_toplevel::ResizeEdge::TopLeft,
        (_, true, true, _) => xdg_toplevel::ResizeEdge::TopRight,
        (true, _, _, true) => xdg_toplevel::ResizeEdge::BottomLeft,
        (_, true, _, true) => xdg_toplevel::ResizeEdge::BottomRight,
        (true, _, _, _) => xdg_toplevel::ResizeEdge::Left,
        (_, true, _, _) => xdg_toplevel::ResizeEdge::Right,
        (_, _, true, _) => xdg_toplevel::ResizeEdge::Top,
        (_, _, _, true) => xdg_toplevel::ResizeEdge::Bottom,
        _ => return None,
    })
}

/// CPU-render the title bar: solid background, rounded top corners, title text,
/// and a "×" close button. `scale` supersamples the buffer (buffer scale =
/// `scale`) so the bar stays crisp on HiDPI outputs; all buffer-space geometry
/// below is in physical pixels.
pub fn render_title_bar(
    width: i32,
    _focused: bool,
    _close_hovered: bool,
    scale: i32,
    title: &str,
    pinned: bool,
    config: &DecorationConfig,
) -> MemoryRenderBuffer {
    let s = scale.max(1);
    let h = config.title_bar_height * s;
    let w = width.max(1) * s;
    let bg = config.bg_color;
    let fg = config.fg_color;
    let cr = ((config.corner_radius * s) as f64)
        .min(w as f64 / 2.0)
        .min(h as f64);

    let mut pixels = vec![0u8; (w * h * 4) as usize];

    // Fill with background color, masking top corners for rounding
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;

            // Corner rounding: compute alpha for top-left and top-right arcs
            let corner_alpha = corner_alpha_at(x, y, w, cr);

            pixels[idx] = (bg[0] as f64 * corner_alpha) as u8;
            pixels[idx + 1] = (bg[1] as f64 * corner_alpha) as u8;
            pixels[idx + 2] = (bg[2] as f64 * corner_alpha) as u8;
            pixels[idx + 3] = (bg[3] as f64 * corner_alpha) as u8;
        }
    }

    // Draw "×" close button: two crossed lines, inset from the right edge
    let btn_size = h;
    let btn_x = w - btn_size - CLOSE_BTN_RIGHT_PAD * s;
    let margin = (btn_size as f64 * 0.37).round() as i32;
    let x0 = btn_x + margin;
    let y0 = margin;
    let x1 = btn_x + btn_size - margin;
    let y1 = h - margin;

    let line_w = btn_size as f64 * CLOSE_BTN_STROKE;
    draw_line(&mut pixels, w, x0, y0, x1, y1, fg, line_w);
    draw_line(&mut pixels, w, x0, y1, x1, y0, fg, line_w);

    // Draw the window title text in the space left of the close button.
    // The left inset matches the close button's effective right inset — its
    // edge padding plus the × glyph's margin inside the button — so the bar
    // looks symmetric.
    // Pinned indicator: a small filled dot near the left edge, fg color,
    // vertically centered. The left-aligned title is then pushed clear of the
    // dot (diameter + gap) so they don't collide.
    let base_left_pad = CLOSE_BTN_RIGHT_PAD * s + margin;
    let left_pad = if pinned {
        let r = (h as f64 * 0.16).round().max(2.0);
        let cx = base_left_pad as f64 + r;
        let cy = h as f64 / 2.0;
        draw_filled_circle(&mut pixels, w, cx, cy, r, fg);
        base_left_pad + (2.0 * r).round() as i32 + TITLE_TEXT_GAP * s
    } else {
        base_left_pad
    };
    let right_limit = btn_x - TITLE_TEXT_GAP * s;
    let available = right_limit - left_pad;
    if available > 0 && !title.is_empty() {
        // `font_size` is in points; convert to logical px (96 dpi) then to
        // buffer px. Not clamped — vertical centering plus the buffer bounds
        // absorb an oversized font.
        let font_px = (config.font_size as f32 * PT_TO_PX * s as f32).max(1.0);
        let (text, text_w) =
            driftwm::text::fit_text(title, &config.font, font_px, config.font_weight, available);
        let origin_x = match config.title_align {
            TitleAlign::Left => left_pad,
            // Centered in the full bar, clamped clear of the close button and
            // the left padding (long titles end up left-aligned + ellipsized).
            TitleAlign::Center => ((w - text_w) / 2).min(right_limit - text_w).max(left_pad),
        };
        driftwm::text::rasterize_into(
            &mut pixels,
            w,
            h,
            &text,
            &config.font,
            font_px,
            config.font_weight,
            fg,
            origin_x,
        );
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        s,
        Transform::Normal,
        None,
    )
}

/// CPU-render a suspended window's body fill: the SSD background color with
/// rounded corners so it tucks inside the border arc instead of poking past it.
/// Only the bottom pair rounds — the title bar always covers the top corners.
/// `scale` supersamples like `render_title_bar`; the fill is premultiplied.
pub fn render_body_fill(
    width: i32,
    height: i32,
    scale: i32,
    config: &DecorationConfig,
) -> MemoryRenderBuffer {
    let s = scale.max(1);
    let w = width.max(1) * s;
    let h = height.max(1) * s;
    let bg = config.bg_color;
    let ba = bg[3] as f64 / 255.0;
    let cr = ((config.corner_radius * s) as f64)
        .min(w as f64 / 2.0)
        .min(h as f64 / 2.0);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let idx = ((y * w + x) * 4) as usize;
            let a = body_corner_alpha_at(x, y, w, h, cr) * ba;
            pixels[idx] = (bg[0] as f64 * a) as u8;
            pixels[idx + 1] = (bg[1] as f64 * a) as u8;
            pixels[idx + 2] = (bg[2] as f64 * a) as u8;
            pixels[idx + 3] = (255.0 * a) as u8;
        }
    }

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        s,
        Transform::Normal,
        None,
    )
}

/// Anti-aliased coverage (1 inside → 0 outside) for a body pixel, rounding only
/// the bottom corners (the title bar covers the top pair). Mirrors
/// `corner_alpha_at`'s smoothstep falloff.
fn body_corner_alpha_at(x: i32, y: i32, w: i32, h: i32, r: f64) -> f64 {
    if r <= 0.0 {
        return 1.0;
    }
    let px = x as f64 + 0.5;
    let py = y as f64 + 0.5;

    let cx = if px < r {
        r
    } else if px > w as f64 - r {
        w as f64 - r
    } else {
        return 1.0;
    };
    let cy = if py > h as f64 - r {
        h as f64 - r
    } else {
        return 1.0;
    };

    let dx = px - cx;
    let dy = py - cy;
    let dist = (dx * dx + dy * dy).sqrt();
    let t = (dist - r + 0.5).clamp(0.0, 1.0);
    1.0 - t * t * (3.0 - 2.0 * t)
}

/// Anti-aliased alpha for top-left and top-right corner rounding.
fn corner_alpha_at(x: i32, y: i32, w: i32, r: f64) -> f64 {
    if r <= 0.0 {
        return 1.0;
    }
    let px = x as f64 + 0.5;
    let py = y as f64 + 0.5;

    // Top-left corner
    if px < r && py < r {
        let dx = r - px;
        let dy = r - py;
        let dist = (dx * dx + dy * dy).sqrt();
        let t = (dist - r + 0.5).clamp(0.0, 1.0);
        return 1.0 - t * t * (3.0 - 2.0 * t);
    }
    // Top-right corner
    let right_edge = w as f64;
    if px > right_edge - r && py < r {
        let dx = px - (right_edge - r);
        let dy = r - py;
        let dist = (dx * dx + dy * dy).sqrt();
        let t = (dist - r + 0.5).clamp(0.0, 1.0);
        return 1.0 - t * t * (3.0 - 2.0 * t);
    }
    1.0
}

/// Draw an anti-aliased filled circle, blending `color` over the buffer with a
/// 1px soft edge. Same straight-alpha blend convention as `draw_line`.
fn draw_filled_circle(pixels: &mut [u8], stride: i32, cx: f64, cy: f64, r: f64, color: [u8; 4]) {
    let height = pixels.len() as i32 / (stride * 4);
    let x_min = (cx - r - 1.0).floor().max(0.0) as i32;
    let x_max = (cx + r + 1.0).ceil().min(stride as f64) as i32;
    let y_min = (cy - r - 1.0).floor().max(0.0) as i32;
    let y_max = (cy + r + 1.0).ceil().min(height as f64) as i32;
    for py in y_min..y_max {
        for px in x_min..x_max {
            let dx = px as f64 + 0.5 - cx;
            let dy = py as f64 + 0.5 - cy;
            let dist = (dx * dx + dy * dy).sqrt();
            let cov = (r + 0.5 - dist).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            let idx = ((py * stride + px) * 4) as usize;
            if idx + 3 >= pixels.len() {
                continue;
            }
            let a = (color[3] as f64 / 255.0 * cov).min(1.0);
            let inv_a = 1.0 - a;
            pixels[idx] = (color[0] as f64 * a + pixels[idx] as f64 * inv_a) as u8;
            pixels[idx + 1] = (color[1] as f64 * a + pixels[idx + 1] as f64 * inv_a) as u8;
            pixels[idx + 2] = (color[2] as f64 * a + pixels[idx + 2] as f64 * inv_a) as u8;
            pixels[idx + 3] =
                (pixels[idx + 3] as f64 + a * 255.0 * (1.0 - pixels[idx + 3] as f64 / 255.0)) as u8;
        }
    }
}

/// Draw an anti-aliased line using distance-from-line rasterization.
#[allow(clippy::too_many_arguments)]
fn draw_line(
    pixels: &mut [u8],
    stride: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: [u8; 4],
    line_width: f64,
) {
    let pad = (line_width * 0.5 + 1.0).ceil() as i32;
    let min_x = x0.min(x1) - pad;
    let max_x = x0.max(x1) + pad;
    let min_y = y0.min(y1) - pad;
    let max_y = y0.max(y1) + pad;

    let dx = (x1 - x0) as f64;
    let dy = (y1 - y0) as f64;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.001 {
        return;
    }

    for py in min_y..=max_y {
        for px in min_x..=max_x {
            let t = ((px as f64 - x0 as f64) * dx + (py as f64 - y0 as f64) * dy) / (len * len);
            let t = t.clamp(0.0, 1.0);
            let proj_x = x0 as f64 + t * dx;
            let proj_y = y0 as f64 + t * dy;
            let dist = ((px as f64 - proj_x).powi(2) + (py as f64 - proj_y).powi(2)).sqrt();
            let aa = (1.0 - (dist - line_width * 0.5).max(0.0) / 0.8).clamp(0.0, 1.0);
            if aa > 0.0 {
                let idx = ((py * stride + px) * 4) as usize;
                if idx + 3 < pixels.len() {
                    let a = (color[3] as f64 / 255.0 * aa).min(1.0);
                    let inv_a = 1.0 - a;
                    pixels[idx] = (color[0] as f64 * a + pixels[idx] as f64 * inv_a) as u8;
                    pixels[idx + 1] = (color[1] as f64 * a + pixels[idx + 1] as f64 * inv_a) as u8;
                    pixels[idx + 2] = (color[2] as f64 * a + pixels[idx + 2] as f64 * inv_a) as u8;
                    pixels[idx + 3] = (pixels[idx + 3] as f64
                        + a * 255.0 * (1.0 - pixels[idx + 3] as f64 / 255.0))
                        as u8;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xdg_toplevel::ResizeEdge;

    fn loc(x: i32, y: i32) -> Point<i32, Logical> {
        Point::from((x, y))
    }
    fn pt(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }
    fn sz(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }

    const PAD: i32 = CLOSE_BTN_RIGHT_PAD;

    #[test]
    fn close_button_rect_dimensions_are_bar_height_by_bar_height() {
        let r = close_button_rect(loc(0, 100), 400, 25);
        assert_eq!(r.size.w, 25);
        assert_eq!(r.size.h, 25);
    }

    #[test]
    fn close_button_rect_top_is_bar_height_above_window_loc() {
        let r = close_button_rect(loc(0, 100), 400, 25);
        assert_eq!(r.loc.y, 100 - 25);
    }

    #[test]
    fn close_button_rect_right_edge_is_width_minus_pad_from_window_left() {
        let r = close_button_rect(loc(50, 200), 400, 25);
        assert_eq!(r.loc.x + r.size.w, 50 + 400 - PAD);
    }

    #[test]
    fn close_button_rect_works_with_negative_canvas_coords() {
        let r = close_button_rect(loc(-300, -100), 200, 25);
        assert_eq!(r.size.w, 25);
        assert_eq!(r.size.h, 25);
        assert_eq!(r.loc.y, -100 - 25);
        assert_eq!(r.loc.x + r.size.w, -300 + 200 - PAD);
    }

    #[test]
    fn title_bar_contains_point_in_bar_body_returns_true() {
        assert!(title_bar_contains(pt(100.0, 80.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_in_close_button_area_returns_false() {
        // title_bar_contains explicitly excludes the close button zone
        assert!(!title_bar_contains(pt(370.0, 80.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_above_bar_returns_false() {
        assert!(!title_bar_contains(pt(100.0, 74.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_at_bar_top_boundary_returns_true() {
        // half-open interval: y >= bar_top is included
        assert!(title_bar_contains(pt(100.0, 75.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_at_bar_bottom_boundary_returns_false() {
        // half-open interval: y >= window_loc.y is excluded (client area)
        assert!(!title_bar_contains(pt(100.0, 100.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_left_of_window_returns_false() {
        assert!(!title_bar_contains(pt(-1.0, 80.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_at_left_boundary_returns_true() {
        assert!(title_bar_contains(pt(0.0, 80.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn title_bar_contains_point_right_of_bar_body_returns_false() {
        let bar_right = 0.0 + 400.0 - 25.0 - PAD as f64;
        assert!(!title_bar_contains(
            pt(bar_right, 80.0),
            loc(0, 100),
            400,
            25
        ));
    }

    #[test]
    fn close_button_contains_point_inside_returns_true() {
        assert!(close_button_contains(pt(370.0, 80.0), loc(0, 100), 400, 25));
    }

    #[test]
    fn close_button_contains_point_in_bar_body_returns_false() {
        assert!(!close_button_contains(
            pt(100.0, 80.0),
            loc(0, 100),
            400,
            25
        ));
    }

    #[test]
    fn close_button_contains_point_above_bar_returns_false() {
        assert!(!close_button_contains(
            pt(370.0, 74.0),
            loc(0, 100),
            400,
            25
        ));
    }

    #[test]
    fn close_button_contains_point_below_bar_returns_false() {
        assert!(!close_button_contains(
            pt(370.0, 100.0),
            loc(0, 100),
            400,
            25
        ));
    }

    #[test]
    fn title_bar_and_close_button_cover_disjoint_regions() {
        let in_close = pt(370.0, 80.0);
        assert!(close_button_contains(in_close, loc(0, 100), 400, 25));
        assert!(!title_bar_contains(in_close, loc(0, 100), 400, 25));

        let in_body = pt(100.0, 80.0);
        assert!(title_bar_contains(in_body, loc(0, 100), 400, 25));
        assert!(!close_button_contains(in_body, loc(0, 100), 400, 25));
    }

    // Layout: window_loc=(100,200), size=300x400, bar=25, border=8
    // Outer bbox: x=[92,408), y=[167,608); interior: x=[100,400), y=[175,600)
    fn edge_at(px: f64, py: f64) -> Option<ResizeEdge> {
        resize_edge_at(pt(px, py), loc(100, 200), sz(300, 400), 25, 8)
    }

    #[test]
    fn resize_edge_inside_window_body_returns_none() {
        assert_eq!(edge_at(200.0, 300.0), None);
    }

    #[test]
    fn resize_edge_inside_title_bar_returns_none() {
        assert_eq!(edge_at(200.0, 185.0), None);
    }

    #[test]
    fn resize_edge_outside_outer_bbox_returns_none() {
        assert_eq!(edge_at(200.0, 100.0), None);
        assert_eq!(edge_at(500.0, 300.0), None);
    }

    #[test]
    fn resize_edge_top_border_above_title_bar_returns_top() {
        // outer_top = 167, inner_top = 175 — point in [167,175) is the top strip
        assert_eq!(edge_at(200.0, 170.0), Some(ResizeEdge::Top));
    }

    #[test]
    fn resize_edge_bottom_border_returns_bottom() {
        assert_eq!(edge_at(200.0, 603.0), Some(ResizeEdge::Bottom));
    }

    #[test]
    fn resize_edge_left_border_returns_left() {
        assert_eq!(edge_at(95.0, 300.0), Some(ResizeEdge::Left));
    }

    #[test]
    fn resize_edge_right_border_returns_right() {
        assert_eq!(edge_at(403.0, 300.0), Some(ResizeEdge::Right));
    }

    #[test]
    fn resize_edge_top_left_corner_returns_top_left() {
        assert_eq!(edge_at(95.0, 170.0), Some(ResizeEdge::TopLeft));
    }

    #[test]
    fn resize_edge_top_right_corner_returns_top_right() {
        assert_eq!(edge_at(403.0, 170.0), Some(ResizeEdge::TopRight));
    }

    #[test]
    fn resize_edge_bottom_left_corner_returns_bottom_left() {
        assert_eq!(edge_at(95.0, 603.0), Some(ResizeEdge::BottomLeft));
    }

    #[test]
    fn resize_edge_bottom_right_corner_returns_bottom_right() {
        assert_eq!(edge_at(403.0, 603.0), Some(ResizeEdge::BottomRight));
    }
}
