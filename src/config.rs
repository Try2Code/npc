//! Configuration, and the roots this tool refuses to mutate.
//!
//! The `.xmp`-only guard in the shim protects individual *files*. This
//! protects whole *trees*: a library listed here is read-only no matter what
//! command is aimed at it. It exists because the drive holding the real
//! library is exFAT, which has no Unix permissions, so `chmod -R a-w` is not
//! available there as a backstop.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Thumbnail sizing. Exposed in config because the right values depend on
/// screen and font size, which the program cannot see and the user can.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct Thumbs {
    /// Long edge, in pixels, for grid tiles.
    pub grid_px: u32,
    /// Long edge for the single-image view.
    pub view_px: u32,
    /// JPEG quality, 1-100. Lower trades file size for detail; at these
    /// resolutions ~80 keeps the cache small without visible artefacts.
    pub quality: u8,
}

impl Default for Thumbs {
    fn default() -> Self {
        // Sized from a 3440x1440 display, which is what this is used on.
        //
        //   grid : 3440 / 5 tiles across = 688 px, and 1147 px if narrowed to
        //          3 across with `-`. 1280 covers the widest tile without
        //          upscaling.
        //   view : deliberately matched to the NEF's 1620x1080 preview tier.
        //          Asking for more forces Exiv2 up to the 6016x4016 tier, so
        //          every frame decodes a 24 MP JPEG and Lanczos-downscales it
        //          -- 140 ms, which is visible stutter when walking a shoot.
        //          At 1620 the tier is used as-is with no resampling at all.
        //          It is softer than the 2100 px a full-screen 3:2 frame could
        //          use; the single view is for choosing, and anything needing
        //          real inspection goes to RawTherapee via `a`.
        //
        // Both are overridable in config.toml; the cache key includes the pixel
        // size, so changing them regenerates exactly what is affected.
        Self {
            grid_px: 1280,
            view_px: 1620,
            quality: 80,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Config {
    /// Absolute paths. Any mutating operation whose target resolves inside one
    /// of these is refused at dispatch, before a file is opened.
    #[serde(default)]
    pub protected_roots: Vec<PathBuf>,
    #[serde(default)]
    pub thumbs: Thumbs,
    /// Where catalogs live. Defaults to the XDG data directory.
    ///
    /// Worth moving if the default lands somewhere you would rather it did
    /// not -- a synced home directory, or a small root partition. A catalog is
    /// rebuildable, so relocating it costs a rescan and nothing else.
    #[serde(default)]
    pub catalog_dir: Option<PathBuf>,
}

impl Config {
    /// Where catalogs are kept, honouring `catalog_dir` if it is set.
    pub fn catalog_store(&self) -> PathBuf {
        if let Some(dir) = &self.catalog_dir {
            return expand_home(dir);
        }
        directories::ProjectDirs::from("", "", "photo-catalog")
            .map(|d| d.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".photo-catalog"))
    }

    /// Where thumbnails are kept, under the same directory when one is
    /// configured -- keeping the two together is what people expect when they
    /// move a catalog off the default path.
    pub fn cache_store(&self) -> PathBuf {
        if let Some(dir) = &self.catalog_dir {
            return expand_home(dir).join("thumbnails");
        }
        directories::ProjectDirs::from("", "", "photo-catalog")
            .map(|d| d.cache_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".photo-cache"))
    }
}

/// `~` is what a person writes in a config file, and nothing expands it for us.
fn expand_home(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    path.to_path_buf()
}

impl Config {
    pub fn path() -> PathBuf {
        directories::ProjectDirs::from("", "", "photo-catalog")
            .map(|d| d.config_dir().join("config.toml"))
            .unwrap_or_else(|| PathBuf::from("photo-catalog.toml"))
    }

    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Refuse if `target` is inside a protected root. Compares canonicalised
    /// paths, so symlinks and `..` cannot walk around the check.
    pub fn ensure_mutable(&self, target: &Path) -> Result<()> {
        let resolved = canonical_or_best_effort(target);

        for root in &self.protected_roots {
            let root_resolved = canonical_or_best_effort(root);
            if resolved.starts_with(&root_resolved) {
                bail!(
                    "{} is inside the protected root {} and this tool will not modify it.\n\
                     Protected roots live in {}.",
                    target.display(),
                    root.display(),
                    Self::path().display()
                );
            }
        }
        Ok(())
    }
}

/// Canonicalise as far as the path exists, keeping the rest. A sidecar that
/// has not been created yet must still be checked against its directory.
fn canonical_or_best_effort(path: &Path) -> PathBuf {
    if let Ok(p) = path.canonicalize() {
        return p;
    }
    let mut existing = path;
    let mut tail = Vec::new();
    while let Some(parent) = existing.parent() {
        if let Some(name) = existing.file_name() {
            tail.push(name.to_os_string());
        }
        if let Ok(p) = parent.canonicalize() {
            let mut out = p;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        existing = parent;
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(roots: &[&str]) -> Config {
        Config {
            protected_roots: roots.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn allows_paths_outside_every_protected_root() {
        let c = cfg(&["/media/ram/MyPics"]);
        assert!(c
            .ensure_mutable(Path::new("/home/ram/src/tasks/test-data/a.xmp"))
            .is_ok());
        assert!(c.ensure_mutable(Path::new("/tmp/scratch/b.xmp")).is_ok());
    }

    #[test]
    fn refuses_the_protected_root_itself_and_anything_under_it() {
        let c = cfg(&["/media/ram/MyPics"]);
        assert!(c.ensure_mutable(Path::new("/media/ram/MyPics")).is_err());
        assert!(c
            .ensure_mutable(Path::new("/media/ram/MyPics/2026/08/750_6926.xmp"))
            .is_err());
    }

    /// A sibling whose name merely starts with the same characters is not
    /// inside the root -- `starts_with` on components, not on strings.
    #[test]
    fn does_not_confuse_a_sibling_with_a_prefix_name() {
        let c = cfg(&["/media/ram/MyPics"]);
        assert!(c
            .ensure_mutable(Path::new("/media/ram/MyPicsBackup/x.xmp"))
            .is_ok());
    }

    #[test]
    fn catalog_dir_defaults_to_xdg_and_is_overridable() {
        let default = Config::default();
        assert!(default.catalog_store().ends_with("photo-catalog"));

        let moved = Config {
            catalog_dir: Some(PathBuf::from("/data/photo-catalogs")),
            ..Default::default()
        };
        assert_eq!(moved.catalog_store(), PathBuf::from("/data/photo-catalogs"));
        // Thumbnails follow the catalog: keeping them apart when someone has
        // deliberately relocated one is not what they meant.
        assert_eq!(
            moved.cache_store(),
            PathBuf::from("/data/photo-catalogs/thumbnails")
        );
    }

    /// `~` is what a person writes in a config file, and nothing expands it
    /// for us -- a literal `~` directory would be created instead.
    #[test]
    fn a_tilde_in_the_catalog_path_is_expanded() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let c = Config {
            catalog_dir: Some(PathBuf::from("~/photo-cat")),
            ..Default::default()
        };
        assert_eq!(c.catalog_store(), PathBuf::from(home).join("photo-cat"));
    }

    #[test]
    fn config_parses_the_documented_shape() {
        let c: Config = toml::from_str(
            r#"
            protected_roots = ["/media/ram/MyPics"]
            catalog_dir = "~/photo-catalogs"

            [thumbs]
            grid_px = 1280
            view_px = 1620
            quality = 80
            "#,
        )
        .unwrap();
        assert_eq!(c.protected_roots.len(), 1);
        assert_eq!(c.thumbs.grid_px, 1280);
        assert!(c.catalog_dir.is_some());
    }

    #[test]
    fn empty_config_protects_nothing() {
        assert!(Config::default()
            .ensure_mutable(Path::new("/anywhere/at/all.xmp"))
            .is_ok());
    }

    /// A path that does not exist yet must still be checked against the
    /// directory it would be created in, or the guard could be bypassed by
    /// naming a file before creating it.
    #[test]
    fn checks_paths_that_do_not_exist_yet() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(&[dir.path().to_str().unwrap()]);
        let unborn = dir.path().join("deep/not/created/yet.xmp");
        assert!(c.ensure_mutable(&unborn).is_err());
    }

    /// ..and symlinks cannot walk out of a protected root either.
    #[test]
    fn symlinks_cannot_escape_the_guard() {
        let protected = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let real = protected.path().join("real.xmp");
        std::fs::write(&real, b"x").unwrap();
        let link = elsewhere.path().join("link.xmp");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let c = cfg(&[protected.path().to_str().unwrap()]);
        assert!(
            c.ensure_mutable(&link).is_err(),
            "a symlink into a protected root must be refused"
        );
    }
}
