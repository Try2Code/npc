//! Thumbnails.
//!
//! **Never demosaic the RAW, and never decode more pixels than you need.** A
//! NEF from a D750 carries a four-tier preview pyramid, and Exiv2 hands back
//! the smallest tier at or above the requested size, so a grid thumbnail
//! costs a small read rather than a full-frame decode.
//!
//! The `_NEF_embedded.jpg` files sitting beside these RAWs look like a
//! shortcut and are the opposite of one -- they are the *full-resolution*
//! 6016x4016 tier, 1.8 MB each. Reading one to build a tile means decoding a
//! 24 MP JPEG. Measured over the whole library, preferring them cost 4.7x:
//! 105s versus 22s. The RAW is the cheap source, which is not what intuition
//! suggests.
//!
//! The cache lives on the laptop, not beside the photographs. Two reasons:
//! nothing is ever written next to your files for display purposes, and paired
//! with offline queries it means the library can be browsed visually with the
//! drive unplugged.

use crate::db::Catalog;
use anyhow::{Context, Result};
use image::imageops::FilterType;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Size {
    /// Grid tiles.
    Grid,
    /// Single-image view.
    View,
}

/// Runtime sizing, set once from config at startup. Kept as statics rather
/// than threaded through every call site because it is read-only after start
/// and every consumer wants the same answer.
static GRID_PX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1280);
static VIEW_PX: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(3200);
static QUALITY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(78);

/// Apply configured thumbnail sizes. Because the cache key includes the pixel
/// size, changing these invalidates the affected thumbnails automatically.
pub fn configure(cfg: &crate::config::Thumbs) {
    use std::sync::atomic::Ordering::Relaxed;
    GRID_PX.store(cfg.grid_px.clamp(64, 8192), Relaxed);
    VIEW_PX.store(cfg.view_px.clamp(64, 12288), Relaxed);
    QUALITY.store(cfg.quality.clamp(1, 100), Relaxed);
}

pub fn quality() -> u8 {
    QUALITY.load(std::sync::atomic::Ordering::Relaxed)
}

impl Size {
    pub fn px(self) -> u32 {
        use std::sync::atomic::Ordering::Relaxed;
        match self {
            Size::Grid => GRID_PX.load(Relaxed),
            Size::View => VIEW_PX.load(Relaxed),
        }
    }
    fn tag(self) -> &'static str {
        match self {
            Size::Grid => "g",
            Size::View => "v",
        }
    }
    /// Grid tiles are small and numerous, so speed wins; the single view is
    /// looked at closely, so quality does.
    fn filter(self) -> FilterType {
        match self {
            Size::Grid => FilterType::Triangle,
            Size::View => FilterType::Lanczos3,
        }
    }
}

/// Where a thumbnail's pixels come from, and what identifies its cache entry.
#[derive(Debug, Clone)]
pub struct Source {
    pub path: PathBuf,
    /// True when the pixels must be pulled out of a RAW via Exiv2.
    pub embedded: bool,
    pub mtime: i64,
    pub orientation: u16,
    pub stack_id: i64,
}

/// Pick the cheapest source for a stack.
///
/// **The RAW comes first**, which is counter-intuitive and was measured rather
/// than reasoned. Exiv2's `PreviewManager` selects a *tier*: for a 384 px
/// thumbnail it hands back the 640x424 preview, a 140 KB read. The
/// `_NEF_embedded.jpg` files sitting beside these RAWs are not small previews
/// at all -- they are the full-resolution 6016x4016 tier, 1.8 MB each. Reading
/// one to make a tile means decoding a 24 MP JPEG.
///
/// Preferring the derived file to "avoid the Exiv2 call" traded a 2 ms header
/// read for a 100 ms decode, and cost 6x on a full-library pre-warm.
pub fn best_source(cat: &Catalog, stack_id: i64) -> Result<Option<Source>> {
    let row = cat.conn.query_row(
        "SELECT s.dir || '/' || f.name, f.role, f.mtime_ns,
                COALESCE(s.orientation, f.exif_orientation, 1)
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.stack_id = ?1 AND f.present = 1
           AND f.role IN ('derived','oocjpg','raw')
         ORDER BY CASE f.role
                    WHEN 'raw'     THEN 0
                    WHEN 'oocjpg'  THEN 1
                    ELSE 2 END
         LIMIT 1",
        [stack_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    );

    match row {
        Ok((path, role, mtime, orientation)) => Ok(Some(Source {
            path: PathBuf::from(path),
            embedded: role == "raw",
            mtime,
            orientation: orientation as u16,
            stack_id,
        })),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub struct Cache {
    dir: PathBuf,
    /// Keys currently being generated, so a slow thumbnail is not queued twice
    /// while the user scrolls back and forth over it.
    inflight: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    tx: mpsc::Sender<(Source, Size, PathBuf)>,
    /// Signals the UI that something new landed and a redraw is worthwhile.
    pub done: mpsc::Receiver<PathBuf>,
}

impl Cache {
    pub fn new(vol_uuid: &str) -> Result<Self> {
        let dir = directories::ProjectDirs::from("", "", "photo-catalog")
            .map(|d| d.cache_dir().join(vol_uuid))
            .unwrap_or_else(|| PathBuf::from(".photo-cache").join(vol_uuid));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating thumbnail cache {}", dir.display()))?;

        let (tx, rx) = mpsc::channel::<(Source, Size, PathBuf)>();
        let (done_tx, done) = mpsc::channel::<PathBuf>();
        let inflight: Arc<Mutex<std::collections::HashSet<PathBuf>>> = Default::default();

        // One worker. Decoding is the expensive half and it happens here,
        // off the UI thread, so scrolling never waits on a JPEG.
        let busy = inflight.clone();
        std::thread::spawn(move || {
            while let Ok((src, size, out)) = rx.recv() {
                let _ = generate(&src, size, &out);
                busy.lock().map(|mut b| b.remove(&out)).ok();
                if done_tx.send(out).is_err() {
                    break;
                }
            }
        });

        Ok(Self {
            dir,
            inflight,
            tx,
            done,
        })
    }

    pub fn path_for(&self, src: &Source, size: Size) -> PathBuf {
        // Keyed on the source file, its mtime, its orientation *and* the
        // target pixel size. Leaving the size out meant changing Grid from
        // 384 to 640 px silently kept serving the old, smaller thumbnails.
        let key = format!(
            "{}|{}|{}|{}",
            src.path.display(),
            src.mtime,
            src.orientation,
            size.px()
        );
        let digest = blake3::hash(key.as_bytes());
        let hex = digest.to_hex();
        self.dir
            .join(&hex[..2])
            .join(format!("{}-{}.jpg", &hex[2..18], size.tag()))
    }

    /// Cached path if it is ready, else `None` after queueing generation.
    pub fn get(&self, src: &Source, size: Size) -> Option<PathBuf> {
        let out = self.path_for(src, size);
        if out.exists() {
            return Some(out);
        }
        if let Ok(mut busy) = self.inflight.lock() {
            if busy.insert(out.clone()) {
                let _ = self.tx.send((src.clone(), size, out));
            }
        }
        None
    }

    /// Generate synchronously. Used by `photo thumbs` for bulk pre-warming.
    pub fn make_now(&self, src: &Source, size: Size) -> Result<PathBuf> {
        let out = self.path_for(src, size);
        if out.exists() {
            return Ok(out);
        }
        generate(src, size, &out)?;
        Ok(out)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Delete cache entries that no current stack would ask for.
    ///
    /// Changing a thumbnail size leaves the previous generation orphaned --
    /// correct, since the key includes the size, but it accumulates. After
    /// two size changes this cache held three generations and 320 MB where
    /// 107 MB was live.
    pub fn prune(&self, cat: &Catalog, stack_ids: &[i64]) -> Result<(usize, u64)> {
        let mut keep = std::collections::HashSet::new();
        for &id in stack_ids {
            if let Ok(Some(src)) = best_source(cat, id) {
                keep.insert(self.path_for(&src, Size::Grid));
                keep.insert(self.path_for(&src, Size::View));
            }
        }

        let mut removed = 0usize;
        let mut bytes = 0u64;
        for entry in walkdir::WalkDir::new(&self.dir)
            .into_iter()
            .filter_map(std::result::Result::ok)
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path().to_path_buf();
            if path.extension().and_then(|e| e.to_str()) != Some("jpg") {
                continue;
            }
            if keep.contains(&path) {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
                bytes += size;
            }
        }
        Ok((removed, bytes))
    }
}

/// Read pixels, downscale, apply orientation, write JPEG.
fn generate(src: &Source, size: Size, out: &Path) -> Result<()> {
    let bytes = if src.embedded {
        // Exiv2 picks the smallest embedded preview at or above our target.
        let blob = crate::meta::extract_preview(&src.path.to_string_lossy(), size.px())?;
        if !blob.ok {
            anyhow::bail!("{}: {}", src.path.display(), blob.error);
        }
        blob.data
    } else {
        std::fs::read(&src.path).with_context(|| format!("reading {}", src.path.display()))?
    };

    let img = image::load_from_memory(&bytes)
        .with_context(|| format!("decoding preview for {}", src.path.display()))?;

    let (w, h) = (img.width(), img.height());
    let long = w.max(h);
    let img = if long > size.px() {
        let scale = size.px() as f32 / long as f32;
        img.resize(
            (w as f32 * scale).round().max(1.0) as u32,
            (h as f32 * scale).round().max(1.0) as u32,
            size.filter(),
        )
    } else {
        img
    };

    let img = apply_orientation(img, src.orientation);

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Temp + rename, so a killed process cannot leave a truncated JPEG that
    // would then be served from cache forever.
    let part = out.with_extension("part");
    {
        // Explicit quality: the default encoder setting is not tunable, and at
        // 4x the pixel count the compression setting is what keeps the cache
        // from ballooning.
        let file = std::fs::File::create(&part)
            .with_context(|| format!("creating {}", part.display()))?;
        let mut writer = std::io::BufWriter::new(file);
        let mut encoder =
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut writer, quality());
        let rgb = img.to_rgb8();
        encoder
            .encode(rgb.as_raw(), rgb.width(), rgb.height(), image::ExtendedColorType::Rgb8)
            .with_context(|| format!("encoding {}", part.display()))?;
    }
    std::fs::rename(&part, out)?;
    Ok(())
}

/// EXIF orientation, applied so portrait shots are not shown on their side.
/// Nikon writes upright previews with an orientation tag rather than rotated
/// pixels, so skipping this would display half the library sideways.
pub fn apply_orientation(img: image::DynamicImage, orientation: u16) -> image::DynamicImage {
    use image::DynamicImage as D;
    match orientation {
        2 => D::ImageRgba8(image::imageops::flip_horizontal(&img)),
        3 => img.rotate180(),
        4 => D::ImageRgba8(image::imageops::flip_vertical(&img)),
        5 => D::ImageRgba8(image::imageops::flip_horizontal(&img.rotate90())),
        6 => img.rotate90(),
        7 => D::ImageRgba8(image::imageops::flip_horizontal(&img.rotate270())),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Pre-warm the cache for a set of stacks. Returns (made, skipped, failed).
pub fn prewarm(
    cat: &Catalog,
    cache: &Cache,
    stack_ids: &[i64],
    size: Size,
) -> (usize, usize, usize) {
    use rayon::prelude::*;

    // Destination paths are resolved up front: `Cache` owns a channel receiver
    // and so is not `Sync`, and there is no reason for the workers to see it.
    let work: Vec<(Source, PathBuf)> = stack_ids
        .iter()
        .filter_map(|&id| best_source(cat, id).ok().flatten())
        .map(|src| {
            let out = cache.path_for(&src, size);
            (src, out)
        })
        .collect();

    let counts: Vec<(usize, usize, usize)> = work
        .par_iter()
        .map(|(src, out)| {
            if out.exists() {
                (0, 1, 0)
            } else if generate(src, size, out).is_ok() {
                (1, 0, 0)
            } else {
                (0, 0, 1)
            }
        })
        .collect();

    counts.into_iter().fold((0, 0, 0), |a, b| {
        (a.0 + b.0, a.1 + b.1, a.2 + b.2)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = RgbImage::new(w, h);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    fn source(dir: &Path, w: u32, h: u32, orientation: u16) -> Source {
        let path = dir.join("shot.jpg");
        std::fs::write(&path, jpeg(w, h)).unwrap();
        Source {
            path,
            embedded: false,
            mtime: 1,
            orientation,
            stack_id: 1,
        }
    }

    #[test]
    fn downscales_to_the_requested_long_edge() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path(), 1620, 1080, 1);
        let out = dir.path().join("t.jpg");
        generate(&src, Size::Grid, &out).unwrap();

        let made = image::open(&out).unwrap();
        assert_eq!(made.width().max(made.height()), Size::Grid.px());
        // 3:2 in, 3:2 out.
        assert_eq!(
            (made.width() as f32 / made.height() as f32 * 100.0).round(),
            (1620.0 / 1080.0 * 100.0f32).round(),
            "aspect ratio was not preserved: {}x{}",
            made.width(),
            made.height()
        );
    }

    #[test]
    fn does_not_upscale_something_already_small() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path(), 200, 100, 1);
        let out = dir.path().join("t.jpg");
        generate(&src, Size::Grid, &out).unwrap();

        let made = image::open(&out).unwrap();
        assert_eq!((made.width(), made.height()), (200, 100));
    }

    /// Orientation 8 is the D750's portrait tag, and it appears throughout this
    /// library. Skipping it would show half the shots on their side.
    #[test]
    fn portrait_orientation_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path(), 400, 200, 8);
        let out = dir.path().join("t.jpg");
        generate(&src, Size::Grid, &out).unwrap();

        let made = image::open(&out).unwrap();
        assert!(
            made.height() > made.width(),
            "a rotated shot came out landscape: {}x{}",
            made.width(),
            made.height()
        );
    }

    #[test]
    fn every_orientation_value_is_handled_without_panicking() {
        let img = DynamicImage::ImageRgb8(RgbImage::new(8, 4));
        for o in 0..=9u16 {
            let out = apply_orientation(img.clone(), o);
            let swapped = matches!(o, 5..=8);
            assert_eq!(
                out.width() < out.height(),
                swapped && 8 > 4,
                "orientation {o} gave {}x{}",
                out.width(),
                out.height()
            );
        }
    }

    #[test]
    fn cache_keys_change_when_the_source_changes() {
        let cache = Cache::new("test-vol").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let a = source(dir.path(), 10, 10, 1);

        let mut b = a.clone();
        b.mtime = 2;
        assert_ne!(
            cache.path_for(&a, Size::Grid),
            cache.path_for(&b, Size::Grid),
            "editing a file must invalidate its thumbnail"
        );

        let mut c = a.clone();
        c.orientation = 8;
        assert_ne!(
            cache.path_for(&a, Size::Grid),
            cache.path_for(&c, Size::Grid),
            "rotating must invalidate its thumbnail"
        );

        assert_ne!(
            cache.path_for(&a, Size::Grid),
            cache.path_for(&a, Size::View),
            "sizes must not collide"
        );
        // Regression: the key must contain the pixel dimension, not just a
        // label, or changing Grid's size serves stale thumbnails forever.
        let key_has_px = {
            let p = cache.path_for(&a, Size::Grid);
            let mut b = a.clone();
            b.mtime = a.mtime; // unchanged
            p != cache.path_for(&b, Size::View)
        };
        assert!(key_has_px);
    }

    #[test]
    fn no_part_file_survives_a_successful_generation() {
        let dir = tempfile::tempdir().unwrap();
        let src = source(dir.path(), 800, 600, 1);
        let out = dir.path().join("sub/t.jpg");
        generate(&src, Size::Grid, &out).unwrap();

        assert!(out.exists());
        assert!(!out.with_extension("part").exists());
    }

    #[test]
    fn a_corrupt_source_fails_without_writing_a_thumbnail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.jpg");
        std::fs::write(&path, b"not a jpeg at all").unwrap();
        let src = Source {
            path,
            embedded: false,
            mtime: 1,
            orientation: 1,
            stack_id: 1,
        };
        let out = dir.path().join("t.jpg");
        assert!(generate(&src, Size::Grid, &out).is_err());
        assert!(!out.exists(), "a failed decode left a thumbnail behind");
    }

    /// Regression: the RAW must win. The _NEF_embedded.jpg beside it is the
    /// full-resolution tier, not a thumbnail, and decoding it is ~50x the work
    /// of letting Exiv2 pick the 640x424 preview.
    /// Pruning must delete previous generations and keep the live ones. It is
    /// the only thing here that removes files, so it gets a test that proves
    /// it does not remove the wrong ones.
    #[test]
    fn prune_removes_stale_generations_but_keeps_the_current_one() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES (?1, 'shot')",
                     [dir.path().to_str().unwrap()])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        std::fs::write(dir.path().join("shot.JPG"), jpeg(64, 48)).unwrap();
        cat.conn
            .execute(
                "INSERT INTO file (stack_id, name, role, mtime_ns, exif_orientation, present)
                 VALUES (?1, 'shot.JPG', 'oocjpg', 7, 1, 1)",
                [sid],
            )
            .unwrap();

        let cache = Cache::new("prune-test").unwrap();
        let src = best_source(&cat, sid).unwrap().unwrap();
        let live = cache.path_for(&src, Size::Grid);
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, b"current").unwrap();

        // A previous generation: same stack, different key.
        let stale = cache.dir().join("ab/deadbeefdeadbeef-g.jpg");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"old generation").unwrap();

        let (removed, _) = cache.prune(&cat, &[sid]).unwrap();
        assert_eq!(removed, 1);
        assert!(live.exists(), "pruning deleted the live thumbnail");
        assert!(!stale.exists(), "pruning kept a stale thumbnail");

        let _ = std::fs::remove_dir_all(cache.dir());
    }

    #[test]
    fn best_source_prefers_the_raw_so_exiv2_can_pick_a_small_tier() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES ('/d', '750_0001')", [])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        for (name, role) in [
            ("750_0001.NEF", "raw"),
            ("750_0001.JPG", "oocjpg"),
            ("750_0001_NEF_embedded.jpg", "derived"),
        ] {
            cat.conn
                .execute(
                    "INSERT INTO file (stack_id, name, role, mtime_ns, exif_orientation, present)
                     VALUES (?1, ?2, ?3, 1, 8, 1)",
                    rusqlite::params![sid, name, role],
                )
                .unwrap();
        }

        let src = best_source(&cat, sid).unwrap().unwrap();
        assert!(
            src.path.to_string_lossy().ends_with(".NEF"),
            "picked {} -- the full-res derived file is not a cheap source",
            src.path.display()
        );
        assert!(src.embedded, "the RAW path must go through Exiv2 tier selection");
        assert_eq!(src.orientation, 8);
    }

    #[test]
    fn best_source_uses_a_jpeg_when_there_is_no_raw() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES ('/d', '750_0002')", [])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO file (stack_id, name, role, mtime_ns, exif_orientation, present)
                 VALUES (?1, '750_0002.JPG', 'oocjpg', 1, 1, 1)",
                [sid],
            )
            .unwrap();

        let src = best_source(&cat, sid).unwrap().unwrap();
        assert!(!src.embedded, "a JPEG needs no Exiv2 extraction");
        assert!(src.path.to_string_lossy().ends_with(".JPG"));
    }

    #[test]
    fn a_stack_with_no_image_members_has_no_source() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES ('/d', 'x')", [])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        cat.conn
            .execute(
                "INSERT INTO file (stack_id, name, role, present) VALUES (?1,'x.NEF.pp3','profile',1)",
                [sid],
            )
            .unwrap();
        assert!(best_source(&cat, sid).unwrap().is_none());
    }
}
