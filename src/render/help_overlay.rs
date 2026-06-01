//! Centered keyboard-shortcuts overlay: internal chrome (not a layer-shell
//! client), so it's a pure render element that input passes through. Toggled
//! by `Action::ToggleHelp` (`state.help_visible`).
//!
//! The content is derived live from `config.keybindings()` so it always
//! reflects the user's actual bindings, including rebinds and `none` unbinds.
//! `exec`/`spawn` rows resolve the program to its absolute path on `$PATH`.
//!
//! The rasterized card is cached per-output keyed by `(signature, w, h, scale)`
//! so a held overlay doesn't re-damage every frame and keep the compositor busy.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::Kind;
use smithay::backend::renderer::element::memory::{
    MemoryRenderBuffer, MemoryRenderBufferRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::input::keyboard::xkb;
use smithay::output::Output;
use smithay::utils::{Physical, Point, Transform};

use driftwm::config::{Action, Direction, FontWeight, KeyCombo};

use super::elements::{OutputRenderElements, PixelSnapRescaleElement};

const TITLE: &str = "driftwm — keyboard shortcuts";

/// Logical text sizes (supersampled by the output scale at render time).
const TITLE_PX: f32 = 17.0;
const HEADER_PX: f32 = 13.0;
const ROW_PX: f32 = 12.5;

/// Logical layout metrics.
const PAD: i32 = 26;
const TITLE_GAP: i32 = 14;
const ROW_H: i32 = 21;
const KEY_DESC_GAP: i32 = 22;
const COL_GAP: i32 = 44;
const DESC_MAX: i32 = 360;
const RADIUS: i32 = 18;
/// Outer margin from the screen edges; bounds how tall the card may grow.
const SCREEN_MARGIN: i32 = 48;

const CARD: [u8; 4] = [0x1e, 0x1e, 0x2e, 0xF2];
const TITLE_COLOR: [u8; 4] = [0xcb, 0xa6, 0xf7, 0xFF];
const HEADER_COLOR: [u8; 4] = [0x89, 0xb4, 0xfa, 0xFF];
const KEY_COLOR: [u8; 4] = [0xa6, 0xe3, 0xa1, 0xFF];
const DESC_COLOR: [u8; 4] = [0xcd, 0xd6, 0xf4, 0xFF];
const PATH_COLOR: [u8; 4] = [0x93, 0x99, 0xb2, 0xFF];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Category {
    Apps,
    Navigation,
    View,
    Windows,
    Zoom,
    System,
}

impl Category {
    fn title(self) -> &'static str {
        match self {
            Category::Apps => "Applications",
            Category::Navigation => "Navigation",
            Category::View => "View",
            Category::Windows => "Windows",
            Category::Zoom => "Zoom",
            Category::System => "System",
        }
    }

    /// Iteration order for sections.
    const ORDER: [Category; 6] = [
        Category::Apps,
        Category::Navigation,
        Category::View,
        Category::Windows,
        Category::Zoom,
        Category::System,
    ];
}

struct Row {
    category: Category,
    keys: String,
    desc: String,
}

/// One laid-out line in a column: either a section header or a binding row.
enum Block {
    Header(String),
    Row { keys: String, desc: String },
}

/// Rasterized overlay cached per output, keyed by `(signature, w, h, scale)`.
pub struct HelpOverlayCache {
    key: (String, i32, i32, i32),
    buffer: MemoryRenderBuffer,
    /// Buffer size in physical px, so the element can be centered on the output.
    size: (i32, i32),
}

/// Build the overlay element, or an empty vec when the overlay is hidden.
pub fn build_help_overlay_elements(
    state: &mut crate::state::DriftWm,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let name = output.name();
    if !state.help_visible || !driftwm::text::fonts_ready() {
        state.render.cached_help_overlay.remove(&name);
        return Vec::new();
    }

    let rows = collect_rows(&state.config);
    if rows.is_empty() {
        state.render.cached_help_overlay.remove(&name);
        return Vec::new();
    }

    let viewport = crate::state::output_logical_size(output);
    let output_scale = output.current_scale().fractional_scale();
    let s = state.decoration_scale.max(1);
    let font = state.config.decorations.font.clone();

    let signature = rows
        .iter()
        .map(|r| format!("{}\u{1}{}\u{1}{}", r.category as u8, r.keys, r.desc))
        .collect::<Vec<_>>()
        .join("\u{2}");

    let avail_h = ((viewport.h - 2 * SCREEN_MARGIN).max(120)) * s;
    let avail_w = ((viewport.w - 2 * SCREEN_MARGIN).max(200)) * s;
    let key = (signature, avail_w, avail_h, s);

    if state.render.cached_help_overlay.get(&name).map(|c| &c.key) != Some(&key) {
        let (buffer, size) = render_overlay(&rows, &font, avail_w, avail_h, s);
        state
            .render
            .cached_help_overlay
            .insert(name.clone(), HelpOverlayCache { key, buffer, size });
    }
    let cache = state.render.cached_help_overlay.get(&name).unwrap();
    let (w_phys, h_phys) = cache.size;

    // Center the card on the output. Geometry above is already in physical px
    // (supersampled), so divide by the buffer scale to get logical, then to
    // physical for the element location.
    let card_w_logical = w_phys / s;
    let card_h_logical = h_phys / s;
    let loc: Point<f64, Physical> = Point::<i32, smithay::utils::Logical>::from((
        (viewport.w - card_w_logical) / 2,
        (viewport.h - card_h_logical) / 2,
    ))
    .to_f64()
    .to_physical(output_scale);

    let Ok(elem) = MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        loc,
        &cache.buffer,
        None,
        None,
        None,
        Kind::Unspecified,
    ) else {
        return Vec::new();
    };

    vec![OutputRenderElements::Decoration(
        PixelSnapRescaleElement::from_element(elem, Point::<i32, Physical>::from((0, 0)), 1.0),
    )]
}

/// Collect every active keybinding into display rows, sorted by category then
/// description for a stable layout (the binding map iterates in hash order).
fn collect_rows(config: &driftwm::config::Config) -> Vec<Row> {
    let mut rows: Vec<Row> = config
        .keybindings()
        .map(|(combo, action)| {
            let (category, desc) = describe_action(action);
            Row {
                category,
                keys: format_combo(combo),
                desc,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        a.category
            .cmp(&b.category)
            .then_with(|| a.desc.cmp(&b.desc))
            .then_with(|| a.keys.cmp(&b.keys))
    });
    rows
}

fn format_combo(combo: &KeyCombo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if combo.modifiers.logo {
        parts.push("Super".into());
    }
    if combo.modifiers.ctrl {
        parts.push("Ctrl".into());
    }
    if combo.modifiers.alt {
        parts.push("Alt".into());
    }
    if combo.modifiers.shift {
        parts.push("Shift".into());
    }
    parts.push(key_name(combo.sym));
    parts.join(" + ")
}

/// Human-friendly name for a keysym. Falls back to xkb's canonical name.
fn key_name(sym: smithay::input::keyboard::Keysym) -> String {
    let raw = xkb::keysym_get_name(sym);
    match raw.as_str() {
        "Return" => "Enter".into(),
        "equal" => "=".into(),
        "minus" => "−".into(),
        "plus" => "+".into(),
        "space" => "Space".into(),
        "Prior" => "PageUp".into(),
        "Next" => "PageDown".into(),
        "Up" => "↑".into(),
        "Down" => "↓".into(),
        "Left" => "←".into(),
        "Right" => "→".into(),
        // Single lowercase letters read better uppercased on a key cap.
        s if s.chars().count() == 1 => s.to_uppercase(),
        s => s.to_string(),
    }
}

fn dir_label(dir: &Direction) -> &'static str {
    match dir {
        Direction::Up => "up",
        Direction::Down => "down",
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::UpLeft => "up-left",
        Direction::UpRight => "up-right",
        Direction::DownLeft => "down-left",
        Direction::DownRight => "down-right",
    }
}

fn describe_action(action: &Action) -> (Category, String) {
    match action {
        Action::Exec(cmd) | Action::Spawn(cmd) => (Category::Apps, describe_exec(cmd)),
        Action::CloseWindow => (Category::Windows, "Close focused window".into()),
        Action::NudgeWindow(d) => (Category::Windows, format!("Nudge window {}", dir_label(d))),
        Action::PanViewport(d) => (
            Category::Navigation,
            format!("Pan viewport {}", dir_label(d)),
        ),
        Action::CenterWindow => (Category::View, "Center focused window".into()),
        Action::CenterNearest(d) => (
            Category::Navigation,
            format!("Focus nearest window {}", dir_label(d)),
        ),
        Action::CycleWindows { backward } => (
            Category::Navigation,
            if *backward {
                "Cycle windows (reverse)".into()
            } else {
                "Cycle windows".into()
            },
        ),
        Action::HomeToggle => (Category::View, "Toggle home (origin)".into()),
        Action::GoToPosition(x, y) => (
            Category::Navigation,
            format!("Jump to bookmark ({x:.0}, {y:.0})"),
        ),
        Action::ZoomIn => (Category::Zoom, "Zoom in".into()),
        Action::ZoomOut => (Category::Zoom, "Zoom out".into()),
        Action::ZoomReset => (Category::Zoom, "Reset zoom to 1.0".into()),
        Action::ZoomToFit => (Category::Zoom, "Zoom to fit all windows".into()),
        Action::ZoomToFitSnapped => (Category::Zoom, "Zoom to fit snapped cluster".into()),
        Action::ToggleFullscreen => (Category::View, "Toggle fullscreen".into()),
        Action::FitWindow => (Category::Windows, "Fit window to viewport".into()),
        Action::FitWindowSnapped => (Category::Windows, "Fit snapped cluster".into()),
        Action::SendToOutput(d) => (
            Category::Windows,
            format!("Send window to {} output", dir_label(d)),
        ),
        Action::FocusCenter => (Category::View, "Focus window at viewport center".into()),
        Action::ReloadConfig => (Category::System, "Reload config".into()),
        Action::ToggleHelp => (Category::System, "Show / hide this help".into()),
        Action::Quit => (Category::System, "Quit driftwm".into()),
    }
}

/// Describe an exec/spawn command, appending the resolved absolute path of the
/// program when it can be found on `$PATH` (the feature the user asked for:
/// see where each launcher binding actually points).
fn describe_exec(cmd: &str) -> String {
    let prog = cmd.split_whitespace().next().unwrap_or("");
    match resolve_on_path(prog) {
        Some(path) => format!("Run {cmd}   ·   {}", path.display()),
        None => format!("Run {cmd}"),
    }
}

/// Resolve a bare program name to the first executable match on `$PATH`.
/// Returns `None` for empty names, paths (already explicit), or no match.
fn resolve_on_path(prog: &str) -> Option<PathBuf> {
    if prog.is_empty() || prog.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(prog);
        let meta = std::fs::metadata(&candidate).ok()?;
        (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(candidate)
    })
}

/// Group the rows into indivisible sections (a header plus its rows), then pack
/// whole sections into newspaper-style columns sized to fit the available
/// height. Keeping a section intact means a header is never separated from its
/// rows across a column break. A single section taller than a column still gets
/// placed (the card grows rather than dropping content).
fn layout_columns(rows: &[Row], lines_per_col: usize) -> Vec<Vec<Block>> {
    let mut sections: Vec<Vec<Block>> = Vec::new();
    for category in Category::ORDER {
        let mut rows_in: Vec<&Row> = rows.iter().filter(|r| r.category == category).collect();
        if rows_in.is_empty() {
            continue;
        }
        rows_in.sort_by(|a, b| a.desc.cmp(&b.desc).then_with(|| a.keys.cmp(&b.keys)));
        let mut section = vec![Block::Header(category.title().to_string())];
        section.extend(rows_in.into_iter().map(|r| Block::Row {
            keys: r.keys.clone(),
            desc: r.desc.clone(),
        }));
        sections.push(section);
    }

    let lines_per_col = lines_per_col.max(1);
    let mut columns: Vec<Vec<Block>> = vec![Vec::new()];
    for section in sections {
        let cur_len = columns.last().unwrap().len();
        // Start a new column if this section wouldn't fit in the current one
        // (unless the column is empty — an oversized section must go somewhere).
        if cur_len > 0 && cur_len + section.len() > lines_per_col {
            columns.push(Vec::new());
        }
        columns.last_mut().unwrap().extend(section);
    }
    columns
}

fn render_overlay(
    rows: &[Row],
    font: &str,
    avail_w: i32,
    avail_h: i32,
    s: i32,
) -> (MemoryRenderBuffer, (i32, i32)) {
    let (pixels, w, h) = render_pixels(rows, font, avail_w, avail_h, s.max(1));
    let buffer = MemoryRenderBuffer::from_slice(
        &pixels,
        Fourcc::Abgr8888,
        (w, h),
        s.max(1),
        Transform::Normal,
        None,
    );
    (buffer, (w, h))
}

/// CPU-render the card to a raw `Abgr8888` pixel buffer (byte order R,G,B,A).
/// Split out from `render_overlay` so it can be exercised without a GPU/renderer.
#[allow(clippy::too_many_arguments)]
fn render_pixels(
    rows: &[Row],
    font: &str,
    avail_w: i32,
    avail_h: i32,
    s: i32,
) -> (Vec<u8>, i32, i32) {
    let s = s.max(1);
    let title_px = TITLE_PX * s as f32;
    let header_px = HEADER_PX * s as f32;
    let row_px = ROW_PX * s as f32;
    let pad = PAD * s;
    let title_gap = TITLE_GAP * s;
    let row_h = ROW_H * s;
    let key_desc_gap = KEY_DESC_GAP * s;
    let col_gap = COL_GAP * s;
    let desc_max = DESC_MAX * s;
    let radius = RADIUS * s;

    let title_h = title_px.ceil() as i32 + title_gap;

    // How many lines fit vertically in one column.
    let body_h = (avail_h - 2 * pad - title_h).max(row_h);
    let lines_per_col = (body_h / row_h).max(1) as usize;

    let columns = layout_columns(rows, lines_per_col);

    // Column inner width: widest key column + gap + widest (clamped) desc.
    let key_w = rows
        .iter()
        .map(|r| driftwm::text::measure(&r.keys, font, row_px, FontWeight::Medium))
        .max()
        .unwrap_or(0);
    let desc_w = rows
        .iter()
        .map(|r| driftwm::text::measure(&r.desc, font, row_px, FontWeight::Normal).min(desc_max))
        .chain(columns.iter().flatten().filter_map(|b| match b {
            Block::Header(h) => Some(driftwm::text::measure(h, font, header_px, FontWeight::Bold)),
            Block::Row { .. } => None,
        }))
        .max()
        .unwrap_or(0)
        .min(desc_max.max(1));
    let col_w = key_w + key_desc_gap + desc_w;

    let n_cols = columns.len().max(1) as i32;
    let title_w = driftwm::text::measure(TITLE, font, title_px, FontWeight::Bold);
    let body_w = n_cols * col_w + (n_cols - 1) * col_gap;

    let w = (2 * pad + body_w.max(title_w)).clamp(1, avail_w.max(1));
    let tallest = columns.iter().map(|c| c.len()).max().unwrap_or(0) as i32;
    let h = (2 * pad + title_h + tallest * row_h).max(1);

    let mut pixels = vec![0u8; (w * h * 4) as usize];
    fill_rounded_rect(&mut pixels, w, h, radius, CARD);

    // Title, vertically centered in its own band at the top.
    rasterize_band(
        &mut pixels,
        w,
        pad,
        title_px.ceil() as i32,
        TITLE,
        font,
        title_px,
        FontWeight::Bold,
        TITLE_COLOR,
        pad,
    );

    let body_top = pad + title_h;
    for (ci, column) in columns.iter().enumerate() {
        let col_x = pad + ci as i32 * (col_w + col_gap);
        let mut y = body_top;
        for block in column {
            match block {
                Block::Header(text) => {
                    rasterize_band(
                        &mut pixels,
                        w,
                        y,
                        row_h,
                        text,
                        font,
                        header_px,
                        FontWeight::Bold,
                        HEADER_COLOR,
                        col_x,
                    );
                }
                Block::Row { keys, desc } => {
                    rasterize_band(
                        &mut pixels,
                        w,
                        y,
                        row_h,
                        keys,
                        font,
                        row_px,
                        FontWeight::Medium,
                        KEY_COLOR,
                        col_x,
                    );
                    let (fitted, _) =
                        driftwm::text::fit_text(desc, font, row_px, FontWeight::Normal, desc_w);
                    // Dim the resolved path portion so the command stands out.
                    let color = if fitted.contains('·') {
                        PATH_COLOR
                    } else {
                        DESC_COLOR
                    };
                    rasterize_band(
                        &mut pixels,
                        w,
                        y,
                        row_h,
                        &fitted,
                        font,
                        row_px,
                        FontWeight::Normal,
                        color,
                        col_x + key_w + key_desc_gap,
                    );
                }
            }
            y += row_h;
        }
    }

    (pixels, w, h)
}

/// Rasterize one text line into the horizontal band `[y0, y0 + band_h)` of the
/// buffer, vertically centered within that band. The band is a contiguous slice
/// of full-width rows, so `text::rasterize_into` can write into it directly.
#[allow(clippy::too_many_arguments)]
fn rasterize_band(
    pixels: &mut [u8],
    w: i32,
    y0: i32,
    band_h: i32,
    text: &str,
    font: &str,
    size: f32,
    weight: FontWeight,
    color: [u8; 4],
    origin_x: i32,
) {
    let start = (y0 * w * 4) as usize;
    let end = ((y0 + band_h) * w * 4) as usize;
    if end > pixels.len() || start >= end {
        return;
    }
    driftwm::text::rasterize_into(
        &mut pixels[start..end],
        w,
        band_h,
        text,
        font,
        size,
        weight,
        color,
        origin_x,
    );
}

/// Fill the buffer with `color`, rounding the four corners (1px feathered) so
/// the card reads as a floating panel rather than a hard rectangle.
fn fill_rounded_rect(pixels: &mut [u8], w: i32, h: i32, radius: i32, color: [u8; 4]) {
    let r = radius.clamp(0, w.min(h) / 2);
    for y in 0..h {
        for x in 0..w {
            let coverage = corner_coverage(x, y, w, h, r);
            if coverage <= 0.0 {
                continue;
            }
            let idx = ((y * w + x) * 4) as usize;
            pixels[idx] = color[0];
            pixels[idx + 1] = color[1];
            pixels[idx + 2] = color[2];
            pixels[idx + 3] = (color[3] as f32 * coverage) as u8;
        }
    }
}

/// Alpha coverage in `[0,1]` for pixel `(x,y)` against a rounded rect, with a
/// 1px feather at the corner arcs for anti-aliasing.
fn corner_coverage(x: i32, y: i32, w: i32, h: i32, r: i32) -> f32 {
    if r == 0 {
        return 1.0;
    }
    let rf = r as f32;
    // Center of the nearest corner arc, if the pixel is in a corner region.
    let cx = if x < r {
        Some(rf)
    } else if x >= w - r {
        Some((w - r) as f32)
    } else {
        None
    };
    let cy = if y < r {
        Some(rf)
    } else if y >= h - r {
        Some((h - r) as f32)
    } else {
        None
    };
    let (Some(cx), Some(cy)) = (cx, cy) else {
        return 1.0; // edges/center: fully covered
    };
    let dist = (((x as f32 + 0.5) - cx).powi(2) + ((y as f32 + 0.5) - cy).powi(2)).sqrt();
    (rf - dist + 0.5).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use driftwm::config::Modifiers;
    use smithay::input::keyboard::Keysym;

    fn combo(logo: bool, shift: bool, sym: Keysym) -> KeyCombo {
        KeyCombo {
            modifiers: Modifiers {
                logo,
                shift,
                ..Modifiers::EMPTY
            },
            sym,
        }
    }

    #[test]
    fn format_combo_orders_mods_then_key() {
        let c = combo(
            true,
            true,
            Keysym::from(smithay::input::keyboard::keysyms::KEY_w),
        );
        assert_eq!(format_combo(&c), "Super + Shift + W");
    }

    #[test]
    fn key_name_prettifies_common_syms() {
        assert_eq!(
            key_name(Keysym::from(smithay::input::keyboard::keysyms::KEY_Return)),
            "Enter"
        );
        assert_eq!(
            key_name(Keysym::from(smithay::input::keyboard::keysyms::KEY_equal)),
            "="
        );
    }

    #[test]
    fn resolve_on_path_rejects_paths_and_empty() {
        assert!(resolve_on_path("").is_none());
        assert!(resolve_on_path("/usr/bin/foot").is_none());
    }

    #[test]
    fn describe_exec_includes_resolved_path() {
        // `sh` is essentially always on PATH in a build/test environment.
        let desc = describe_exec("sh -c true");
        assert!(desc.starts_with("Run sh -c true"));
    }

    #[test]
    fn layout_splits_into_columns_when_tall() {
        // Several sections, each small, with a tight per-column budget: they
        // should spill into multiple columns.
        let cats = [
            Category::Apps,
            Category::Navigation,
            Category::View,
            Category::Windows,
        ];
        let rows: Vec<Row> = cats
            .iter()
            .flat_map(|&c| {
                (0..3).map(move |i| Row {
                    category: c,
                    keys: format!("Super + {i}"),
                    desc: format!("action {i}"),
                })
            })
            .collect();
        let cols = layout_columns(&rows, 5);
        assert!(
            cols.len() >= 2,
            "expected multiple columns, got {}",
            cols.len()
        );
    }

    #[test]
    fn layout_keeps_sections_intact() {
        // A header must always be followed by at least its first row in the same
        // column — sections are never split across a column break.
        let cats = [Category::Apps, Category::Navigation, Category::Windows];
        let rows: Vec<Row> = cats
            .iter()
            .flat_map(|&c| {
                (0..4).map(move |i| Row {
                    category: c,
                    keys: format!("K{i}"),
                    desc: format!("d{i}"),
                })
            })
            .collect();
        for col in layout_columns(&rows, 5) {
            for (i, block) in col.iter().enumerate() {
                if matches!(block, Block::Header(_)) {
                    assert!(
                        matches!(col.get(i + 1), Some(Block::Row { .. })),
                        "header not followed by its rows in the same column"
                    );
                }
            }
        }
    }

    /// Render the real overlay (default bindings) to a PNG for visual review.
    /// Ignored by default — needs system fonts. Run with:
    ///   cargo test --lib -- --ignored render_overlay_to_png --nocapture
    #[test]
    #[ignore]
    fn render_overlay_to_png() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let d2 = done.clone();
        driftwm::text::warm_fonts(move || d2.store(true, Ordering::SeqCst));
        for _ in 0..200 {
            if driftwm::text::fonts_ready() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(driftwm::text::fonts_ready(), "fonts never warmed");
        let _ = done;

        let config = driftwm::config::Config::from_toml("").expect("default config");
        let rows = collect_rows(&config);
        assert!(!rows.is_empty());
        let s = 2;
        let (pixels, w, h) = render_pixels(&rows, &config.decorations.font, 1800 * s, 1100 * s, s);
        let img = image::RgbaImage::from_raw(w as u32, h as u32, pixels).expect("buffer");
        let path = "/tmp/help_overlay.png";
        img.save(path).expect("save png");
        eprintln!("wrote {path} ({w}x{h})");
    }
}
