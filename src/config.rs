//! Configuration, and the roots this tool refuses to mutate.
//!
//! The `.xmp`-only guard in the shim protects individual *files*. This
//! protects whole *trees*: a library listed here is read-only no matter what
//! command is aimed at it. It exists because the drive holding the real
//! library is exFAT, which has no Unix permissions, so `chmod -R a-w` is not
//! available there as a backstop.

use anyhow::{bail, Context, Result};
use ratatui::style::Color;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The TUI's palette, collapsed to the handful of roles the screen actually
/// distinguishes -- not one slot per call site, which today's code would
/// need 44 of. Each role is deserialized by `ratatui::style::Color` itself
/// (named colors, `"#rrggbb"`, or a 256-color index as a string), so nothing
/// here has to reparse what that crate already parses correctly.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Colors {
    /// Which panel has keyboard focus, and the cursor within one (the
    /// sidebar's border, the timeline's marker/header/bars, the grid tile
    /// under the cursor, the query text badge).
    pub focus: Color,
    /// A marked or notable value: ratings, the exposure line, the offline
    /// badge, a multi-selected grid tile, the list's selection mark, the
    /// timeline's cursor bar.
    pub accent: Color,
    /// Secondary or dim text: counts, roles, an unfocused border, the status
    /// line.
    pub muted: Color,
    /// A stack marked flagged.
    pub ok: Color,
    /// A stack marked rejected.
    pub warn: Color,
    /// The tag list next to a filename.
    pub tag: Color,
    /// The single most prominent line on screen: the current filename.
    pub text: Color,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            focus: Color::Cyan,
            accent: Color::Yellow,
            muted: Color::DarkGray,
            ok: Color::Green,
            warn: Color::Red,
            tag: Color::Blue,
            text: Color::White,
        }
    }
}

/// The native GUI's theme. Deliberately just this one on/off switch rather
/// than `Colors`' seven roles -- egui's built-in dark/light `Visuals` already
/// covers every widget's chrome consistently, so there is no per-role palette
/// worth inventing here. Sets only the *starting* theme: an in-app toggle
/// changes the running session's look without writing back here, since
/// nothing in this tool writes its own config file today.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Gui {
    pub theme: Theme,
}

impl Default for Gui {
    fn default() -> Self {
        Self { theme: Theme::Dark }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    Dark,
    Light,
}

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

/// The background library watcher (`tui`/`npcg` only): notices a file that
/// appears outside the dated `YYYY/MM/DD` import tree -- typically a
/// reprocessed export saved back into the library by RawTherapee/GIMP/
/// darktable -- and catalogs it in place, the same as running `npc scan`
/// yourself would. See `src/watch.rs`'s module doc for the full design and
/// why this is a scoped `scan()`, not `import`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Watch {
    pub enabled: bool,
    /// How long a directory must go quiet before it's rescanned -- long
    /// enough that an editor's multi-file export batch, or a RAW+JPG pair
    /// arriving moments apart, settles into one scan instead of several.
    pub debounce_ms: u64,
    /// Extra top-level directory names to never watch, beyond the two
    /// always-excluded ones (the `YYYY` dated tree, and `.trash`).
    pub exclude: Vec<String>,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 2000,
            exclude: Vec::new(),
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
    /// The TUI's palette. See [`Colors`] for what each role covers.
    #[serde(default)]
    pub colors: Colors,
    /// The native GUI's starting theme. See [`Gui`].
    #[serde(default)]
    pub gui: Gui,
    /// The background library watcher. See [`Watch`].
    #[serde(default)]
    pub watch: Watch,
    /// The library this tool works on when no root is given on the command line.
    ///
    /// Without it every command would need an explicit path, and hardcoding
    /// one makes the binary useless on any other machine -- which is exactly
    /// what it was doing.
    #[serde(default)]
    pub library: Option<PathBuf>,
    /// Where catalogs live. Defaults to the XDG data directory.
    ///
    /// Worth moving if the default lands somewhere you would rather it did
    /// not -- a synced home directory, or a small root partition. A catalog is
    /// rebuildable, so relocating it costs a rescan and nothing else.
    #[serde(default)]
    pub catalog_dir: Option<PathBuf>,
    /// Default destination for `npc export`, used when `-o`/`--out` is not
    /// given on the command line. Same "one central place instead of typing
    /// it every time" role `library` plays for `--root`.
    #[serde(default)]
    pub export_dir: Option<PathBuf>,
}

impl Config {
    /// The configured library, with `~` expanded.
    pub fn library_root(&self) -> Option<PathBuf> {
        self.library.as_deref().map(expand_home)
    }

    /// The configured default export destination, with `~` expanded.
    pub fn default_export_dir(&self) -> Option<PathBuf> {
        self.export_dir.as_deref().map(expand_home)
    }

    /// Where catalogs are kept, honouring `catalog_dir` if it is set.
    pub fn catalog_store(&self) -> PathBuf {
        if let Some(dir) = &self.catalog_dir {
            return expand_home(dir);
        }
        directories::ProjectDirs::from("", "", "npc")
            .map_or_else(|| PathBuf::from(".npc"), |d| d.data_dir().to_path_buf())
    }

    /// Where thumbnails are kept, under the same directory when one is
    /// configured -- keeping the two together is what people expect when they
    /// move a catalog off the default path.
    pub fn cache_store(&self) -> PathBuf {
        if let Some(dir) = &self.catalog_dir {
            return expand_home(dir).join("thumbnails");
        }
        directories::ProjectDirs::from("", "", "npc").map_or_else(
            || PathBuf::from(".npc-cache"),
            |d| d.cache_dir().to_path_buf(),
        )
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
        directories::ProjectDirs::from("", "", "npc").map_or_else(
            || PathBuf::from("npc.toml"),
            |d| d.config_dir().join("config.toml"),
        )
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
    fn the_library_root_comes_from_config_and_expands_home() {
        assert!(Config::default().library_root().is_none());

        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let c = Config {
            library: Some(PathBuf::from("~/Pictures/library")),
            ..Default::default()
        };
        assert_eq!(
            c.library_root(),
            Some(PathBuf::from(home).join("Pictures/library"))
        );
    }

    #[test]
    fn export_dir_defaults_to_none_and_expands_home() {
        assert!(Config::default().default_export_dir().is_none());

        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
        let c = Config {
            export_dir: Some(PathBuf::from("~/Pictures/graded")),
            ..Default::default()
        };
        assert_eq!(
            c.default_export_dir(),
            Some(PathBuf::from(home).join("Pictures/graded"))
        );
    }

    #[test]
    fn catalog_dir_defaults_to_xdg_and_is_overridable() {
        let default = Config::default();
        assert!(default.catalog_store().ends_with("npc"));

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
            r##"
            protected_roots = ["/media/ram/MyPics"]
            catalog_dir = "~/photo-catalogs"

            [thumbs]
            grid_px = 1280
            view_px = 1620
            quality = 80

            [colors]
            accent = "magenta"
            warn = "#ff0000"
            "##,
        )
        .unwrap();
        assert_eq!(c.protected_roots.len(), 1);
        assert_eq!(c.thumbs.grid_px, 1280);
        assert!(c.catalog_dir.is_some());
        assert_eq!(c.colors.accent, Color::Magenta);
        assert_eq!(c.colors.warn, Color::Rgb(255, 0, 0));
        // Roles left unset in [colors] still get their own default, not a
        // blanket fallback to whatever was set elsewhere in the table.
        assert_eq!(c.colors.focus, Color::Cyan);
    }

    /// A `[colors]` table naming only the roles someone actually wants to
    /// change is the whole point -- the rest must fall back to the same
    /// defaults `Colors::default()` gives an absent table entirely.
    #[test]
    fn colors_default_when_the_section_is_absent() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.colors.focus, Color::Cyan);
        assert_eq!(c.colors.accent, Color::Yellow);
        assert_eq!(c.colors.muted, Color::DarkGray);
        assert_eq!(c.colors.ok, Color::Green);
        assert_eq!(c.colors.warn, Color::Red);
        assert_eq!(c.colors.tag, Color::Blue);
        assert_eq!(c.colors.text, Color::White);
    }

    #[test]
    fn an_unknown_color_name_is_rejected_with_a_useful_message() {
        let err = toml::from_str::<Config>("[colors]\naccent = \"not-a-color\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not-a-color"), "got: {err}");
    }

    #[test]
    fn gui_theme_defaults_to_dark_and_parses_light() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.gui.theme, Theme::Dark);

        let c: Config = toml::from_str("[gui]\ntheme = \"light\"").unwrap();
        assert_eq!(c.gui.theme, Theme::Light);
    }

    #[test]
    fn watch_defaults_to_enabled_with_a_two_second_debounce() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.watch.enabled);
        assert_eq!(c.watch.debounce_ms, 2000);
        assert!(c.watch.exclude.is_empty());

        let c: Config =
            toml::from_str("[watch]\nenabled = false\ndebounce_ms = 500\nexclude = [\"inbox\"]")
                .unwrap();
        assert!(!c.watch.enabled);
        assert_eq!(c.watch.debounce_ms, 500);
        assert_eq!(c.watch.exclude, vec!["inbox".to_string()]);
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
