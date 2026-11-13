//! Decal registry: images pinned to the canvas (see [`driftwm::decals`]).
//!
//! Mutations save at once rather than through the session debounce: the file is
//! tiny, changes only on an explicit command, and a decal lost to a crash right
//! after being dropped would be a surprise.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use smithay::backend::renderer::element::texture::TextureBuffer;
use smithay::backend::renderer::gles::GlesTexture;
use smithay::utils::{Logical, Point, Rectangle, Size};

use driftwm::decals::{self, Decal, MAX_DECALS};

use super::DriftWm;

/// Largest image file a decal accepts. Decoding happens on the render path, so
/// this bounds how long a frame can stall on one.
pub(crate) const MAX_DECAL_FILE_BYTES: u64 = 32 << 20;

/// Longest side, in pixels, of a decal image.
pub(crate) const MAX_DECAL_SIDE: u32 = 8192;

#[derive(Default)]
pub struct DecalStore {
    /// Where decals are saved. `None` disables persistence.
    pub path: Option<PathBuf>,
    /// Bottom→top.
    pub items: Vec<Decal>,
    next_id: u64,
    /// Pixel size of each decal image, read from the file header when the decal
    /// is placed or loaded. A decal without one (the file vanished) is not drawn
    /// and cannot be hit.
    sizes: HashMap<PathBuf, Size<i32, Logical>>,
    /// Uploaded images by path, filled by the renderer. The buffer is kept across
    /// frames so its element id is stable and the damage tracker sees a still
    /// image as still. `None` is a file that failed to decode, remembered so it
    /// is not decoded again every frame.
    pub(crate) textures: HashMap<PathBuf, Option<TextureBuffer<GlesTexture>>>,
}

impl DecalStore {
    /// The canvas rect (internal, Y-down) of `decal`.
    pub(crate) fn rect_of(&self, decal: &Decal) -> Option<Rectangle<f64, Logical>> {
        let size = *self.sizes.get(&decal.path)?;
        let (w, h) = (size.w as f64 * decal.scale, size.h as f64 * decal.scale);
        Some(Rectangle::new(
            (decal.position[0] - w / 2.0, -decal.position[1] - h / 2.0).into(),
            (w, h).into(),
        ))
    }

    pub(crate) fn image_size(&self, path: &Path) -> Option<Size<i32, Logical>> {
        self.sizes.get(path).copied()
    }

    fn remember_size(&mut self, path: &Path) -> Result<(), String> {
        let size = check_image_file(path)?;
        self.sizes.insert(path.to_path_buf(), size);
        Ok(())
    }
}

/// How a new decal is sized.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum DecalSize {
    /// Multiplier on the image's pixel size.
    Scale(f64),
    /// Canvas units for the image's longest side.
    Fit(f64),
}

impl DriftWm {
    /// Read the saved decals at startup.
    pub fn load_decals(&mut self) {
        let Some(path) = self.decals.path.clone() else {
            return;
        };
        self.decals.items = decals::read(&path);
        for decal in self.decals.items.clone() {
            if let Err(err) = self.decals.remember_size(&decal.path) {
                tracing::warn!("decals: {err}");
            }
        }
        self.decals.next_id = self
            .decals
            .items
            .iter()
            .map(|d| d.id)
            .max()
            .map_or(1, |m| m + 1);
    }

    pub(crate) fn add_decal(
        &mut self,
        path: PathBuf,
        position: [f64; 2],
        size: DecalSize,
    ) -> Result<u64, String> {
        if self.decals.items.len() >= MAX_DECALS {
            return Err(format!(
                "at most {MAX_DECALS} decals can be placed; remove one first"
            ));
        }
        let image = check_image_file(&path)?;
        let scale = match size {
            DecalSize::Scale(scale) => scale,
            DecalSize::Fit(fit) => fit / image.w.max(image.h) as f64,
        };
        if let Some(problem) = decals::problem(position, scale) {
            return Err(problem);
        }
        self.decals.sizes.insert(path.clone(), image);
        let id = self.decals.next_id.max(1);
        self.decals.next_id = id + 1;
        self.decals.items.push(Decal {
            id,
            path,
            position,
            scale,
        });
        self.decals_changed();
        Ok(id)
    }

    pub(crate) fn remove_decal(&mut self, id: u64) -> Result<(), String> {
        let index = self
            .decals
            .items
            .iter()
            .position(|d| d.id == id)
            .ok_or_else(|| format!("no decal with id {id}"))?;
        let removed = self.decals.items.remove(index);
        if !self.decals.items.iter().any(|d| d.path == removed.path) {
            self.decals.sizes.remove(&removed.path);
            self.decals.textures.remove(&removed.path);
        }
        self.decals_changed();
        Ok(())
    }

    /// The topmost decal whose rect holds `point` (internal canvas, Y-down).
    /// A decal not drawn yet has no known size and is never hit.
    pub(crate) fn decal_at(&self, point: Point<f64, Logical>) -> Option<u64> {
        self.decals
            .items
            .iter()
            .rev()
            .find(|d| {
                self.decals
                    .rect_of(d)
                    .is_some_and(|rect| rect.contains(point))
            })
            .map(|d| d.id)
    }

    /// Put decal `id`'s center at `position` (Y-up) and save.
    pub(crate) fn move_decal(&mut self, id: u64, position: [f64; 2]) -> Result<(), String> {
        self.place_decal(id, position)?;
        self.save_decals();
        Ok(())
    }

    /// [`Self::move_decal`] without the save, for a drag that moves a decal on
    /// every pointer event and saves once when it lets go.
    pub(crate) fn place_decal(&mut self, id: u64, position: [f64; 2]) -> Result<(), String> {
        let decal = self
            .decals
            .items
            .iter_mut()
            .find(|d| d.id == id)
            .ok_or_else(|| format!("no decal with id {id}"))?;
        if let Some(problem) = decals::problem(position, decal.scale) {
            return Err(problem);
        }
        decal.position = position;
        self.mark_all_dirty();
        Ok(())
    }

    fn decals_changed(&mut self) {
        self.save_decals();
        self.mark_all_dirty();
    }

    pub(crate) fn save_decals(&mut self) {
        if let Some(path) = &self.decals.path
            && let Err(err) = decals::write(path, &self.decals.items)
        {
            tracing::warn!("decals: could not save {}: {err}", path.display());
        }
    }

    /// The Y-up position of decal `id`, if it exists.
    pub(crate) fn decal_position(&self, id: u64) -> Option<[f64; 2]> {
        self.decals
            .items
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.position)
    }
}

/// Reject what the render path could not (or should not) load, at the command
/// that names the file, where the error reaches the user. Returns the image's
/// pixel size, read from its header.
fn check_image_file(path: &Path) -> Result<Size<i32, Logical>, String> {
    if !path.is_absolute() {
        return Err("decal path must be absolute".to_string());
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX_DECAL_FILE_BYTES {
        return Err(format!(
            "{} is over the {} MiB decal limit",
            path.display(),
            MAX_DECAL_FILE_BYTES >> 20
        ));
    }
    let (w, h) = image::image_dimensions(path)
        .map_err(|e| format!("{} is not a readable image: {e}", path.display()))?;
    if w == 0 || h == 0 || w > MAX_DECAL_SIDE || h > MAX_DECAL_SIDE {
        return Err(format!(
            "{} is {w}x{h}; decals are at most {MAX_DECAL_SIDE} px a side",
            path.display()
        ));
    }
    Ok(Size::from((w as i32, h as i32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_path_is_refused() {
        assert!(check_image_file(Path::new("a.png")).is_err());
    }

    #[test]
    fn a_missing_file_is_refused() {
        assert!(check_image_file(Path::new("/nonexistent/driftwm/a.png")).is_err());
    }

    #[test]
    fn a_directory_is_refused() {
        assert!(check_image_file(Path::new("/")).is_err());
    }
}
