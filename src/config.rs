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
        //   view : a 3:2 frame in a 3440x1400 window is height-limited, so the
        //          long edge needed is 1400 * 1.5 = 2100 px. 2560 leaves room
        //          for wider crops without paying for a 3200 px cache.
        //
        // Both are overridable in config.toml; the cache key includes the pixel
        // size, so changing them regenerates exactly what is affected.
        Self {
            grid_px: 1280,
            view_px: 2560,
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
        assert!(c.ensure_mutable(Path::new("/home/ram/src/tasks/test-data/a.xmp")).is_ok());
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
        assert!(c.ensure_mutable(Path::new("/media/ram/MyPicsBackup/x.xmp")).is_ok());
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
