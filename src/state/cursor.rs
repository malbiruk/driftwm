use std::collections::HashMap;
use std::time::Instant;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::input::pointer::CursorImageStatus;
use smithay::utils::{Logical, Point, Transform};

/// All animation frames for a loaded xcursor, at a single nominal size.
pub struct CursorFrames {
    /// (buffer, hotspot, delay_ms) per frame.
    pub frames: Vec<(MemoryRenderBuffer, Point<i32, Logical>, u32)>,
    /// Sum of all frame delays. 0 = static cursor (single frame or all delays zero).
    pub total_duration_ms: u32,
}

/// Cursor-related state: current image, grab ownership, animation cache.
pub struct CursorState {
    pub cursor_status: CursorImageStatus,
    /// True while a compositor grab (pan/resize) owns the cursor icon.
    pub grab_cursor: bool,
    /// Cursor warp target from a locked pointer's position hint (canvas coords).
    pub pointer_position_hint: Option<Point<f64, Logical>>,
    /// True while the pointer is over an SSD decoration area.
    pub decoration_cursor: bool,
    pub cursor_buffers: HashMap<String, CursorFrames>,
    /// Loading cursor show time.
    pub exec_cursor_show_at: Option<Instant>,
    /// Loading cursor deadline.
    pub exec_cursor_deadline: Option<Instant>,
}

impl CursorState {
    pub fn new() -> Self {
        Self {
            cursor_status: CursorImageStatus::default_named(),
            grab_cursor: false,
            pointer_position_hint: None,
            decoration_cursor: false,
            cursor_buffers: HashMap::new(),
            exec_cursor_show_at: None,
            exec_cursor_deadline: None,
        }
    }

    /// True if the current cursor is an animated xcursor (multiple frames with delays).
    pub fn is_animated(&self) -> bool {
        let name = match &self.cursor_status {
            CursorImageStatus::Named(icon) => icon.name(),
            _ => return false,
        };
        self.cursor_buffers
            .get(name)
            .is_some_and(|cf| cf.total_duration_ms > 0)
    }

    /// Load all xcursor animation frames by name and cache them.
    /// Returns a reference to the cached `CursorFrames`.
    pub fn load_xcursor(&mut self, name: &str) -> Option<&CursorFrames> {
        if !self.cursor_buffers.contains_key(name) {
            let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into());
            let theme = xcursor::CursorTheme::load(&theme_name);
            let path = theme.load_icon(name)?;
            let data = std::fs::read(path).ok()?;
            let images = xcursor::parser::parse_xcursor(&data)?;

            let target_size = std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(24);

            let best_size = images
                .iter()
                .map(|img| img.size)
                .min_by_key(|&s| (s as i32 - target_size as i32).unsigned_abs())?;

            let mut frames = Vec::new();
            let mut total_delay: u32 = 0;
            for img in &images {
                if img.size != best_size {
                    continue;
                }
                let buffer = MemoryRenderBuffer::from_slice(
                    &img.pixels_rgba,
                    Fourcc::Argb8888,
                    (img.width as i32, img.height as i32),
                    1,
                    Transform::Normal,
                    None,
                );
                let hotspot = Point::from((img.xhot as i32, img.yhot as i32));
                frames.push((buffer, hotspot, img.delay));
                total_delay = total_delay.saturating_add(img.delay);
            }

            if frames.is_empty() {
                return None;
            }

            let total_duration_ms = if frames.len() == 1 || total_delay == 0 {
                0
            } else {
                total_delay
            };

            self.cursor_buffers.insert(
                name.to_string(),
                CursorFrames {
                    frames,
                    total_duration_ms,
                },
            );
        }
        self.cursor_buffers.get(name)
    }
}
