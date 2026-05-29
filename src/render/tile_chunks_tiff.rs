//! Pyramidal tiled TIFF source for chunked tile-bg rendering.
//!
//! Each IFD ("page") in the file is one LOD level. Page 0 is full resolution;
//! each subsequent page is a downsampled copy. Tiles within a page are read
//! lazily via `read_tile(lod, cx, cy)`.
//!
//! Validation refuses stripped TIFFs and unsupported color types (only
//! RGB8 / RGBA8 are accepted) so we never silently degrade or panic later.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use tiff::ColorType;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::{PlanarConfiguration, Tag};

#[derive(Debug, Clone, Copy)]
pub struct LodMetadata {
    pub image_dims: (u32, u32),
    pub tile_dims: (u32, u32),
    color: TiffColor,
}

#[derive(Debug, Clone, Copy)]
enum TiffColor {
    Rgb8,
    Rgba8,
}

pub struct TiffSource {
    path: PathBuf,
    decoder: Decoder<BufReader<File>>,
    lods: Vec<LodMetadata>,
    current_lod: u32,
}

pub struct DecodedTile {
    pub rgba: Vec<u8>,
    /// Actual tile dimensions after edge cropping (≤ `LodMetadata.tile_dims`).
    pub width: u32,
    pub height: u32,
}

impl TiffSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, String> {
        let path = path.as_ref().to_path_buf();
        let lods = scan_lods(&path)?;
        let decoder = make_decoder(&path)?;
        Ok(Self {
            path,
            decoder,
            lods,
            current_lod: 0,
        })
    }

    pub fn lods(&self) -> &[LodMetadata] {
        &self.lods
    }

    /// `read_chunk` returns a tight `actual_w * actual_h * samples` buffer for
    /// edge tiles (the crate strips internal tile padding), so the returned
    /// `DecodedTile` carries the cropped dims rather than `tile_dims`.
    pub fn read_tile(&mut self, lod: u32, cx: u32, cy: u32) -> Result<DecodedTile, String> {
        let lod_idx = lod as usize;
        let meta = self
            .lods
            .get(lod_idx)
            .copied()
            .ok_or_else(|| format!("LOD {lod} out of range (have {})", self.lods.len()))?;
        let tiles_across = meta.image_dims.0.div_ceil(meta.tile_dims.0);
        let tiles_down = meta.image_dims.1.div_ceil(meta.tile_dims.1);
        if cx >= tiles_across || cy >= tiles_down {
            return Err(format!(
                "tile ({cx},{cy}) out of range at LOD {lod} ({tiles_across}x{tiles_down})"
            ));
        }

        self.navigate_to_lod(lod)?;
        let tile_index = cy * tiles_across + cx;
        let chunk = self
            .decoder
            .read_chunk(tile_index)
            .map_err(|e| format!("read_chunk({tile_index}) at LOD {lod}: {e}"))?;
        let raw = match chunk {
            DecodingResult::U8(v) => v,
            other => return Err(format!("non-u8 sample type at LOD {lod}: {other:?}")),
        };

        let width = (meta.image_dims.0 - cx * meta.tile_dims.0).min(meta.tile_dims.0);
        let height = (meta.image_dims.1 - cy * meta.tile_dims.1).min(meta.tile_dims.1);
        let bpp_raw = bytes_per_pixel(meta.color);
        let expected_len = (width as usize) * (height as usize) * bpp_raw;
        if raw.len() != expected_len {
            return Err(format!(
                "tile ({cx},{cy}) at LOD {lod}: decoded {} bytes, expected {expected_len}",
                raw.len()
            ));
        }
        Ok(DecodedTile {
            rgba: rgb_to_rgba8(raw, meta.color),
            width,
            height,
        })
    }

    /// IFDs are a singly-linked list — no API to seek backward, so a backward
    /// jump rebuilds the decoder. Cheap for adjacent-LOD access; if a future
    /// loader randomizes LOD access, cache IFD offsets and seek directly.
    fn navigate_to_lod(&mut self, lod: u32) -> Result<(), String> {
        if lod < self.current_lod {
            self.decoder = make_decoder(&self.path)?;
            self.current_lod = 0;
        }
        while self.current_lod < lod {
            self.decoder
                .next_image()
                .map_err(|e| format!("next_image to LOD {lod}: {e}"))?;
            self.current_lod += 1;
        }
        Ok(())
    }
}

fn make_decoder(path: &Path) -> Result<Decoder<BufReader<File>>, String> {
    let file = File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    Decoder::new(BufReader::new(file)).map_err(|e| format!("not a TIFF ({}): {e}", path.display()))
}

fn scan_lods(path: &Path) -> Result<Vec<LodMetadata>, String> {
    let mut decoder = make_decoder(path)?;
    let mut lods = Vec::new();
    loop {
        lods.push(read_lod_metadata(&mut decoder)?);
        if !decoder.more_images() {
            break;
        }
        decoder
            .next_image()
            .map_err(|e| format!("next_image during scan: {e}"))?;
    }
    Ok(lods)
}

fn read_lod_metadata(decoder: &mut Decoder<BufReader<File>>) -> Result<LodMetadata, String> {
    if decoder.get_chunk_type() != ChunkType::Tile {
        return Err("page is stripped, not tiled — pyramidal TIFF must be tiled".into());
    }
    // PlanarConfiguration is optional; absence = Chunky (TIFF default, samples
    // interleaved per pixel). Planar puts each sample in its own plane, which
    // our RGBA-interleaved render path can't consume.
    let planar = decoder
        .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)
        .map_err(|e| format!("PlanarConfiguration tag: {e}"))?
        .unwrap_or(PlanarConfiguration::Chunky as u16);
    if planar != PlanarConfiguration::Chunky as u16 {
        return Err("planar TIFF (PlanarConfiguration=Planar) not supported".into());
    }
    let image_dims = decoder
        .dimensions()
        .map_err(|e| format!("dimensions: {e}"))?;
    let tile_dims = decoder.chunk_dimensions();
    if image_dims.0 == 0 || image_dims.1 == 0 {
        return Err(format!("zero image dimension {image_dims:?}"));
    }
    if tile_dims.0 == 0 || tile_dims.1 == 0 {
        return Err(format!("zero tile dimension {tile_dims:?}"));
    }
    let color = match decoder.colortype().map_err(|e| format!("colortype: {e}"))? {
        ColorType::RGB(8) => TiffColor::Rgb8,
        ColorType::RGBA(8) => TiffColor::Rgba8,
        other => {
            return Err(format!(
                "unsupported color type {other:?} (need RGB8 or RGBA8)"
            ));
        }
    };
    Ok(LodMetadata {
        image_dims,
        tile_dims,
        color,
    })
}

fn bytes_per_pixel(color: TiffColor) -> usize {
    match color {
        TiffColor::Rgb8 => 3,
        TiffColor::Rgba8 => 4,
    }
}

fn rgb_to_rgba8(raw: Vec<u8>, color: TiffColor) -> Vec<u8> {
    match color {
        TiffColor::Rgba8 => raw,
        TiffColor::Rgb8 => {
            let mut out = Vec::with_capacity(raw.len() / 3 * 4);
            for px in raw.chunks_exact(3) {
                out.extend_from_slice(&[px[0], px[1], px[2], 255]);
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    /// The tiff crate's encoder can't produce tiled output, so we can only
    /// exercise the negative path here; positive open() / read_tile() coverage
    /// is deferred to a real vips-produced fixture in phase 4+.
    fn write_stripped_rgb8_tiff(path: &Path) {
        use tiff::encoder::TiffEncoder;
        use tiff::encoder::colortype::RGB8;
        let mut buf = Cursor::new(Vec::new());
        let mut enc = TiffEncoder::new(&mut buf).unwrap();
        let data: Vec<u8> = (0..4 * 4 * 3).map(|i| i as u8).collect();
        enc.write_image::<RGB8>(4, 4, &data).unwrap();
        let bytes = buf.into_inner();
        let mut f = File::create(path).unwrap();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
    }

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("driftwm-tiff-test-{}-{name}.tif", std::process::id()));
        p
    }

    #[test]
    fn rejects_stripped_tiff() {
        let path = tmp_path("stripped");
        write_stripped_rgb8_tiff(&path);
        let err = TiffSource::open(&path)
            .err()
            .expect("stripped TIFF must be rejected");
        assert!(err.contains("stripped"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rejects_missing_file() {
        let err = TiffSource::open("/nonexistent/driftwm-tiff-missing.tif")
            .err()
            .expect("missing file must be rejected");
        assert!(err.contains("open"), "got: {err}");
    }

    #[test]
    fn rejects_non_tiff_file() {
        let path = tmp_path("not-a-tiff");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"this is not a tiff").unwrap();
        f.sync_all().unwrap();
        let err = TiffSource::open(&path)
            .err()
            .expect("non-TIFF must be rejected");
        assert!(err.contains("not a TIFF"), "got: {err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn rgb_to_rgba8_upconverts_rgb() {
        let raw = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        let out = rgb_to_rgba8(raw, TiffColor::Rgb8);
        assert_eq!(out, vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255]);
    }

    #[test]
    fn rgb_to_rgba8_passes_rgba_through() {
        let raw = vec![10, 20, 30, 40, 50, 60, 70, 80];
        let out = rgb_to_rgba8(raw.clone(), TiffColor::Rgba8);
        assert_eq!(out, raw);
    }
}
