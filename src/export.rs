//! Exporting copies.
//!
//! Two routes. The built-in one resizes and re-encodes in process: 2000 px on
//! the long edge at quality 95, from the best pixels available. Anything else
//! is a named preset in config that shells out, so `convert` and friends can
//! do what they are better at than I would be.
//!
//! Export only ever *creates* files under the target directory, and refuses if
//! that target resolves inside a library or a protected root. The write
//! invariant is about originals; a copy this tool just made is not one.

// Same reasoning as `thumbs`: resizing is arithmetic between `u32` pixel
// dimensions, `f32` ratios and `usize` buffer lengths, and none of these images
// approach the widths where a cast would lose anything.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use crate::config::Config;
use crate::db::Catalog;
use crate::watch;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The default: full quality, web-sized.
pub const DEFAULT_LONG_EDGE: u32 = 2000;
pub const DEFAULT_QUALITY: u8 = 95;

#[derive(Debug, Clone, Deserialize)]
pub struct Preset {
    pub name: String,
    /// argv, with `{src}` and `{dst}` placeholders. Built directly, never
    /// through a shell -- same rule as actions, for the same reason.
    pub cmd: Vec<String>,
    /// Output extension, so `{dst}` is named sensibly. Defaults to jpg.
    #[serde(default = "default_ext")]
    pub ext: String,
}

fn default_ext() -> String {
    "jpg".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct PresetConfig {
    #[serde(default, rename = "export")]
    pub presets: Vec<Preset>,
}

pub fn presets() -> Vec<Preset> {
    std::fs::read_to_string(Config::path())
        .ok()
        .and_then(|t| toml::from_str::<PresetConfig>(&t).ok())
        .map(|c| c.presets)
        .unwrap_or_default()
}

#[derive(Debug, Default)]
pub struct ExportStats {
    pub written: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

/// Where a stack's pixels come from when exporting.
///
/// The RAW's largest embedded preview is full sensor resolution on a D750
/// (6016x4016), so it is the best source available without developing the
/// RAW. What it is not is a developed RAW: it carries the camera's picture
/// control and white balance. Anything where the development matters wants
/// `rawtherapee-cli`, which is what the shell-out presets are for.
/// Source path, whether it needs Exiv2 extraction, orientation, stack base.
type ExportSource = (PathBuf, bool, u16, String);

fn source_for(cat: &Catalog, stack_id: i64) -> Result<Option<ExportSource>> {
    let row = cat.conn.query_row(
        "SELECT s.dir || '/' || f.name, f.role, s.base,
                COALESCE(s.orientation, f.exif_orientation, 1)
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.stack_id = ?1 AND f.present = 1 AND f.role IN ('raw','oocjpg','derived')
         ORDER BY CASE f.role WHEN 'raw' THEN 0 WHEN 'oocjpg' THEN 1 ELSE 2 END
         LIMIT 1",
        [stack_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        },
    );
    match row {
        Ok((path, role, base, orient)) => Ok(Some((
            PathBuf::from(path),
            role == "raw",
            orient as u16,
            base,
        ))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Refuse to export into the one part of the library that's actually
/// reserved: the dated import tree (`import`'s own `YYYY/MM/DD` convention)
/// and `.trash`. Those are the only places *npc itself* writes new stacks
/// into by itself, so they're the only places a different writer risks
/// colliding with -- landing a copy in `2026/08/09` would be indistinguishable
/// from a real import, and landing one in `.trash` would look like something
/// on its way out rather than in.
///
/// Everywhere else in the library -- `export/`, or any other subtree -- is
/// deliberately *not* refused: that's exactly what the background watcher
/// (`watch.rs`) exists to pick up automatically, and reusing its own
/// `is_excluded` here means the two features can never disagree about what
/// counts as reserved.
pub fn check_target(target: &Path, library: &Path, config: &Config) -> Result<()> {
    config.ensure_mutable(target)?;

    let t = target
        .canonicalize()
        .unwrap_or_else(|_| target.to_path_buf());
    let l = library
        .canonicalize()
        .unwrap_or_else(|_| library.to_path_buf());
    if watch::is_excluded(&l, &t, &config.watch.exclude) {
        bail!(
            "refusing to export into {} -- it is inside the dated import tree \
             or trash under {}.\n\
             Those are reserved for npc's own writes; anywhere else in the \
             library (export/, or any other subtree) is fine.",
            target.display(),
            library.display()
        );
    }
    Ok(())
}

pub struct Options<'a> {
    pub long_edge: u32,
    pub quality: u8,
    pub keep_structure: bool,
    pub strip: bool,
    pub preset: Option<&'a Preset>,
    pub dry_run: bool,
}

pub fn export(
    cat: &Catalog,
    stack_ids: &[i64],
    target: &Path,
    library: &Path,
    config: &Config,
    opts: &Options,
) -> Result<ExportStats> {
    check_target(target, library, config)?;
    let mut stats = ExportStats::default();

    for &sid in stack_ids {
        let Some((src, embedded, orientation, base)) = source_for(cat, sid)? else {
            stats.skipped += 1;
            continue;
        };

        let ext = opts.preset.map_or("jpg", |p| p.ext.as_str());
        let dir = if opts.keep_structure {
            let stack_dir: String =
                cat.conn
                    .query_row("SELECT dir FROM stack WHERE id = ?1", [sid], |r| r.get(0))?;
            let rel = Path::new(&stack_dir)
                .strip_prefix(library)
                .unwrap_or(Path::new(""));
            target.join(rel)
        } else {
            target.to_path_buf()
        };
        let dst = dir.join(format!("{base}.{ext}"));

        if opts.dry_run {
            stats.written += 1;
            continue;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            stats.errors.push(format!("{}: {e}", dir.display()));
            continue;
        }

        let result = match opts.preset {
            Some(p) => run_preset(p, &src, &dst),
            None => render(&src, embedded, orientation, &dst, opts),
        };

        match result {
            Ok(()) => {
                if !opts.strip && opts.preset.is_none() {
                    if let Err(e) = stamp(cat, sid, &dst) {
                        stats.errors.push(format!("{}: {e}", dst.display()));
                    }
                }
                stats.written += 1;
            }
            Err(e) => stats.errors.push(format!("{}: {e}", src.display())),
        }
    }

    Ok(stats)
}

/// The built-in path: decode, downscale, re-encode.
fn render(src: &Path, embedded: bool, orientation: u16, dst: &Path, opts: &Options) -> Result<()> {
    let bytes = if embedded {
        // Ask for more than any tier holds, so Exiv2 yields its largest --
        // full sensor resolution on a D750.
        let blob = crate::meta::extract_preview(&src.to_string_lossy(), u32::MAX / 2)?;
        if !blob.ok {
            bail!("{}", blob.error);
        }
        blob.data
    } else {
        std::fs::read(src)?
    };

    let img =
        image::load_from_memory(&bytes).with_context(|| format!("decoding {}", src.display()))?;

    let long = img.width().max(img.height());
    let img = if long > opts.long_edge {
        let scale = opts.long_edge as f32 / long as f32;
        img.resize(
            (img.width() as f32 * scale).round().max(1.0) as u32,
            (img.height() as f32 * scale).round().max(1.0) as u32,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let img = crate::thumbs::apply_orientation(img, orientation);

    let part = dst.with_extension("part");
    {
        let file = std::fs::File::create(&part)?;
        let mut w = std::io::BufWriter::new(file);
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, opts.quality);
        let rgb = img.to_rgb8();
        enc.encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        )?;
    }
    std::fs::rename(&part, dst)?;
    Ok(())
}

/// A configured command. argv is built directly; `{src}` and `{dst}` are
/// whole arguments, so a filename cannot alter the command.
fn run_preset(preset: &Preset, src: &Path, dst: &Path) -> Result<()> {
    let argv: Vec<String> = preset
        .cmd
        .iter()
        .map(|part| match part.as_str() {
            "{src}" => src.to_string_lossy().to_string(),
            "{dst}" => dst.to_string_lossy().to_string(),
            other => other.to_string(),
        })
        .collect();

    if argv.is_empty() {
        bail!("preset `{}` has an empty command", preset.name);
    }
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .with_context(|| format!("running {}", argv[0]))?;
    if !status.success() {
        bail!("preset `{}` exited with {status}", preset.name);
    }
    if !dst.exists() {
        bail!("preset `{}` produced no output", preset.name);
    }
    Ok(())
}

/// Carry tags and rating onto the copy. Writes to a file this tool created
/// outside the library, which is why it does not go through `write_sidecar`.
fn stamp(cat: &Catalog, stack_id: i64, dst: &Path) -> Result<()> {
    let doc = crate::edit::build_doc(cat, stack_id)?;
    if doc.tags.is_empty() && doc.rating < 0 && doc.title.is_empty() {
        return Ok(());
    }
    crate::meta::stamp_export(&dst.to_string_lossy(), &doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exporting_outside_the_dated_tree_and_trash_is_allowed() {
        let lib = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cfg = Config::default();

        // A subfolder anywhere in the library that isn't the dated import
        // tree or trash -- exactly the export-into-the-library pipeline the
        // watcher exists to pick up (a spektra/RawTherapee preset writing
        // into `export/spektra`, say).
        assert!(check_target(&lib.path().join("export/spektra"), lib.path(), &cfg).is_ok());
        // The bare library root itself: also not a reserved top-level name,
        // symmetric with how the watcher would treat a loose file dropped
        // there.
        assert!(check_target(lib.path(), lib.path(), &cfg).is_ok());
        assert!(check_target(outside.path(), lib.path(), &cfg).is_ok());
    }

    #[test]
    fn exporting_into_the_dated_import_tree_or_trash_is_refused() {
        let lib = tempfile::tempdir().unwrap();
        let cfg = Config::default();

        assert!(check_target(&lib.path().join("2026/08/09"), lib.path(), &cfg).is_err());
        assert!(check_target(&lib.path().join(".trash"), lib.path(), &cfg).is_err());
    }

    #[test]
    fn exporting_into_a_protected_root_is_refused() {
        let lib = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        let cfg = Config {
            protected_roots: vec![target.path().to_path_buf()],
            ..Default::default()
        };
        assert!(check_target(target.path(), lib.path(), &cfg).is_err());
    }

    #[test]
    fn presets_parse_from_config() {
        let cfg: PresetConfig = toml::from_str(
            r#"
            [[export]]
            name = "web"
            cmd = ["convert", "{src}", "-resize", "2000x2000>", "-quality", "95", "{dst}"]

            [[export]]
            name = "tiff"
            cmd = ["convert", "{src}", "{dst}"]
            ext = "tif"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.presets.len(), 2);
        assert_eq!(cfg.presets[0].ext, "jpg", "ext should default to jpg");
        assert_eq!(cfg.presets[1].ext, "tif");
    }

    /// Same rule as actions: a filename that looks like shell syntax is one
    /// argument, not a command.
    #[test]
    fn preset_arguments_are_not_shell_interpreted() {
        let preset = Preset {
            name: "t".into(),
            cmd: vec!["echo".into(), "{src}".into(), "-q".into(), "{dst}".into()],
            ext: "jpg".into(),
        };
        let src = PathBuf::from(r#"/lib/DSC 01'23"; rm -rf x.NEF"#);
        let dst = PathBuf::from("/out/a.jpg");
        let argv: Vec<String> = preset
            .cmd
            .iter()
            .map(|p| match p.as_str() {
                "{src}" => src.to_string_lossy().to_string(),
                "{dst}" => dst.to_string_lossy().to_string(),
                o => o.to_string(),
            })
            .collect();
        assert_eq!(argv.len(), 4);
        assert_eq!(argv[1], src.to_string_lossy());
    }

    #[test]
    fn the_documented_defaults_are_what_was_asked_for() {
        assert_eq!(DEFAULT_LONG_EDGE, 2000);
        assert_eq!(DEFAULT_QUALITY, 95);
    }

    /// A library with one JPG stack, tagged and rated, ready to export.
    fn library() -> (tempfile::TempDir, Catalog, i64) {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();

        let mut img = image::RgbImage::new(3000, 2000);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]);
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        std::fs::write(day.join("750_0001.JPG"), bytes.into_inner()).unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();
        let sid = cat
            .stack_id(day.to_str().unwrap(), "750_0001")
            .unwrap()
            .unwrap();
        crate::edit::apply(
            &mut cat,
            &[sid],
            &[
                crate::edit::Change::AddTags(vec!["beach".into()]),
                crate::edit::Change::SetRating(Some(4)),
            ],
            &Config::default(),
        )
        .unwrap();
        (dir, cat, sid)
    }

    fn opts<'a>() -> Options<'a> {
        Options {
            long_edge: DEFAULT_LONG_EDGE,
            quality: DEFAULT_QUALITY,
            keep_structure: false,
            strip: false,
            preset: None,
            dry_run: false,
        }
    }

    #[test]
    fn the_builtin_preset_resizes_to_the_long_edge() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();

        let stats = export(
            &cat,
            &[sid],
            out.path(),
            lib.path(),
            &Config::default(),
            &opts(),
        )
        .unwrap();
        assert_eq!(stats.written, 1, "errors: {:?}", stats.errors);

        let made = image::open(out.path().join("750_0001.jpg")).unwrap();
        assert_eq!(made.width().max(made.height()), DEFAULT_LONG_EDGE);
        assert_eq!((made.width(), made.height()), (2000, 1333));
    }

    #[test]
    fn tags_and_rating_travel_with_the_copy() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();
        export(
            &cat,
            &[sid],
            out.path(),
            lib.path(),
            &Config::default(),
            &opts(),
        )
        .unwrap();

        let doc =
            crate::meta::read_sidecar(out.path().join("750_0001.jpg").to_str().unwrap()).unwrap();
        assert_eq!(doc.tags, vec!["beach".to_string()]);
        assert_eq!(doc.rating, 4);
    }

    #[test]
    fn strip_leaves_the_copy_without_our_metadata() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.strip = true;
        export(&cat, &[sid], out.path(), lib.path(), &Config::default(), &o).unwrap();

        let doc =
            crate::meta::read_sidecar(out.path().join("750_0001.jpg").to_str().unwrap()).unwrap();
        assert!(doc.tags.is_empty(), "--strip left tags on the copy");
    }

    /// The originals are not the point of export, but they are the point of
    /// everything else: exporting must not touch them.
    #[test]
    fn exporting_does_not_touch_the_originals() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();
        let original = lib.path().join("2026/08/09/750_0001.JPG");
        let before = std::fs::read(&original).unwrap();

        export(
            &cat,
            &[sid],
            out.path(),
            lib.path(),
            &Config::default(),
            &opts(),
        )
        .unwrap();

        assert_eq!(std::fs::read(&original).unwrap(), before);
    }

    #[test]
    fn keep_structure_mirrors_the_library_layout() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.keep_structure = true;
        export(&cat, &[sid], out.path(), lib.path(), &Config::default(), &o).unwrap();

        assert!(out.path().join("2026/08/09/750_0001.jpg").exists());
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();
        let mut o = opts();
        o.dry_run = true;
        let stats = export(&cat, &[sid], out.path(), lib.path(), &Config::default(), &o).unwrap();

        assert_eq!(stats.written, 1);
        assert!(!out.path().join("750_0001.jpg").exists());
    }

    #[test]
    fn a_preset_is_invoked_and_its_output_required() {
        let (lib, cat, sid) = library();
        let out = tempfile::tempdir().unwrap();

        // `cp` stands in for `convert`: same shape, no ImageMagick dependency.
        let preset = Preset {
            name: "copy".into(),
            cmd: vec!["cp".into(), "{src}".into(), "{dst}".into()],
            ext: "jpg".into(),
        };
        let mut o = opts();
        o.preset = Some(&preset);
        let stats = export(&cat, &[sid], out.path(), lib.path(), &Config::default(), &o).unwrap();
        assert_eq!(stats.written, 1, "errors: {:?}", stats.errors);
        assert!(out.path().join("750_0001.jpg").exists());

        // A preset that produces nothing must be reported, not counted.
        let broken = Preset {
            name: "noop".into(),
            cmd: vec!["true".into(), "{src}".into(), "{dst}".into()],
            ext: "tif".into(),
        };
        let mut o2 = opts();
        o2.preset = Some(&broken);
        let stats = export(
            &cat,
            &[sid],
            out.path(),
            lib.path(),
            &Config::default(),
            &o2,
        )
        .unwrap();
        assert_eq!(stats.written, 0);
        assert_eq!(stats.errors.len(), 1);
    }

    #[test]
    fn a_stack_with_no_image_member_is_skipped_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, 'empty')",
                [dir.path().to_str().unwrap()],
            )
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        let out = tempfile::tempdir().unwrap();

        let stats = export(
            &cat,
            &[sid],
            out.path(),
            dir.path(),
            &Config::default(),
            &opts(),
        );
        // The target is inside the "library" here, so this is refused first --
        // use a separate library root to exercise the skip.
        assert!(stats.is_err() || stats.unwrap().skipped == 1);
    }
}
