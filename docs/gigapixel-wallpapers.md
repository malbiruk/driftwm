# Gigapixel wallpapers

driftwm's canvas is infinite: windows float on an unbounded plane that you pan
across and zoom out from. That leaves room for a background far larger than one
screen — a gigapixel image can act as the canvas itself, something you pan over
and zoom out to take in, rather than a fixed screen-sized wallpaper.

## Why a tiled pyramidal TIFF

An ordinary PNG/JPG background is uploaded to the GPU as a single texture, which
maxes out around 8K–16K pixels per side. Anything larger won't fit — and that
doesn't take a literal gigapixel; a whole-world map or a large panorama is
already over the line.

Instead it needs to be **tiled** (cut into small squares) and **pyramidal**
(stored at several progressively smaller copies). driftwm loads only the tiles in
view, and as you zoom out it switches to a smaller copy — so it never has to hold
the whole image at full resolution. A tiled pyramidal TIFF packs all of that into
one file.

## Converting an image

The simplest route is [libvips](https://www.libvips.org/):

```bash
vips tiffsave input.jpg output.tif --tile --pyramid --bigtiff --compression=deflate
```

Then point your config at the result:

```toml
[background]
type = "tile"
path = "~/Pictures/output.tif"
```

`--compression=deflate` is lossless. Don't use `--compression=jpeg` — it's lossy
and produces visible block artifacts at tile edges as you zoom in.

### Alternative: GDAL

If your source is already a GeoTIFF or you work with GIS tools, GDAL produces an
equivalent tiled pyramid (a Cloud-Optimized GeoTIFF _is_ a tiled pyramidal TIFF):

```bash
gdal_translate -of GTiff -co TILED=YES -co COMPRESS=DEFLATE input.tif output.tif
gdaladdo -r average output.tif 2 4 8 16 32
```

## Where to find images

You just need a real **downloadable file** — a large JPEG, PNG, or TIFF past ~16K
on a side. Skip zoom viewers that only stream tiles: Google Arts & Culture, the
Rijksmuseum *Night Watch* viewer, and most of GigaPan don't hand you the whole
image. Beyond that it's down to taste — the canvas tiles infinitely, but on
something this large the repeat is far off-screen, so a non-seamless edge rarely
matters.

Some sources:

- **World & satellite maps** — NASA's
  [Blue Marble](https://science.nasa.gov/earth/earth-observatory/the-blue-marble-true-color-global-imagery-at-1km-resolution/)
  is a public-domain whole-Earth image up to 43200 × 21600 px (TIFF/JPG). Maps
  suit the canvas nicely — panning around one feels like exploring.
- **Wikimedia Commons** — [Large images](https://commons.wikimedia.org/wiki/Category:Large_images)
  and [Gigapixel images](https://commons.wikimedia.org/wiki/Category:Gigapixel_images):
  a big pool of maps, panoramas, and scans in the 16K–40K range, each with its
  license on its own page (many public domain or CC). If a download only gives you a
  thumbnail, see [downloading very large files](https://commons.wikimedia.org/wiki/Commons:Very_high-resolution_file_downloads).
  Skip the *Google Art Project* subcategory — tile sets, not single files.
- **Astronomy** — ESA/Hubble's [Andromeda mosaic](https://esahubble.org/images/heic2501a/)
  is 42208 × 9870 px under CC BY 4.0 (credit "ESA/Hubble"); NASA imagery is public
  domain. Many sky panoramas are only published small or viewer-only, so
  downloadable giant ones are fewer.

Then run your pick through the conversion step above.
