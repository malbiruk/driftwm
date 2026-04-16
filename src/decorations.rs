use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::backend::renderer::gles::element::PixelShaderElement;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Size, Transform};

use driftwm::config::DecorationConfig;

/// Per-window SSD decoration state.
pub struct WindowDecoration {
    pub title_bar: MemoryRenderBuffer,
    pub width: i32,
    pub focused: bool,
    pub close_hovered: bool,
    /// Cached shadow shader element (stable Id for damage tracking). Rebuilt on resize.
    pub cached_shadow: Option<PixelShaderElement>,
    /// Window content size the cached shadow was built for.
    pub shadow_content_size: (i32, i32),
    /// Last post-zoom physical (body, shadow) rect pair used to build the
    /// shadow uniforms — used to skip uniform refreshes on fully static frames.
    pub last_shadow_phys_key: Option<crate::render::ShadowPhysKey>,
}

/// What the pointer is over in SSD decoration space.
#[derive(Debug, Clone, Copy)]
pub enum DecorationHit {
    TitleBar,
    CloseButton,
    ResizeBorder(xdg_toplevel::ResizeEdge),
}

impl WindowDecoration {
    pub fn new(width: i32, focused: bool, config: &DecorationConfig) -> Self {
        let title_bar = render_title_bar(width, focused, false, config);
        Self {
            title_bar,
            width,
            focused,
            close_hovered: false,
            cached_shadow: None,
            shadow_content_size: (0, 0),
            last_shadow_phys_key: None,
        }
    }

    /// Re-render if width or focus changed. Returns true if buffer was rebuilt.
    pub fn update(&mut self, width: i32, focused: bool, config: &DecorationConfig) -> bool {
        if width == self.width && focused == self.focused {
            return false;
        }
        self.width = width;
        self.focused = focused;
        self.title_bar = render_title_bar(width, focused, self.close_hovered, config);
        true
    }
}

/// Right padding so the close button doesn't sit flush with the title bar edge.
const CLOSE_BTN_RIGHT_PAD: i32 = 8;

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

/// CPU-render the title bar: solid background with rounded top corners + "×" close button.
pub fn render_title_bar(
    width: i32,
    _focused: bool,
    _close_hovered: bool,
    config: &DecorationConfig,
) -> MemoryRenderBuffer {
    let h = DecorationConfig::TITLE_BAR_HEIGHT;
    let w = width.max(1);
    let bg = config.bg_color;
    let fg = config.fg_color;
    let cr = config.corner_radius as f64;

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
    let btn_x = w - btn_size - CLOSE_BTN_RIGHT_PAD;
    let margin = (btn_size as f64 * 0.37).round() as i32;
    let x0 = btn_x + margin;
    let y0 = margin;
    let x1 = btn_x + btn_size - margin;
    let y1 = h - margin;

    draw_line(&mut pixels, w, x0, y0, x1, y1, fg);
    draw_line(&mut pixels, w, x0, y1, x1, y0, fg);

    MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        1,
        Transform::Normal,
        None,
    )
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
        return (r - dist + 0.5).clamp(0.0, 1.0);
    }
    // Top-right corner
    let right_edge = w as f64;
    if px > right_edge - r && py < r {
        let dx = px - (right_edge - r);
        let dy = r - py;
        let dist = (dx * dx + dy * dy).sqrt();
        return (r - dist + 0.5).clamp(0.0, 1.0);
    }
    1.0
}

/// Draw an anti-aliased line using distance-from-line rasterization.
fn draw_line(pixels: &mut [u8], stride: i32, x0: i32, y0: i32, x1: i32, y1: i32, color: [u8; 4]) {
    let min_x = x0.min(x1) - 1;
    let max_x = x0.max(x1) + 1;
    let min_y = y0.min(y1) - 1;
    let max_y = y0.max(y1) + 1;

    let dx = (x1 - x0) as f64;
    let dy = (y1 - y0) as f64;
    let len = (dx * dx + dy * dy).sqrt();
    if len < 0.001 {
        return;
    }

    let line_width = 0.8;

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
