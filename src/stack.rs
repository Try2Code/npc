//! Classifying files into stacks.
//!
//! A **stack** is one shot: a directory plus a basename, holding however many
//! files. This is the unit tags and ratings attach to, because the sidecar
//! naming convention (`750_6926.xmp`) is keyed on basename and is therefore
//! shared by every file of the shot.
//!
//! The subtlety is that a basename alone is not enough. This library contains
//! `750_6926_NEF_embedded.jpg` -- an already-extracted preview -- and naive
//! matching would make it a separate stack, duplicating a real one. In the
//! test library that is 1182 phantom stacks. Derived suffixes get stripped
//! back to the parent.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The original: `750_6926.NEF`
    Raw,
    /// Out-of-camera JPG, what culling happens on: `750_6926.JPG`
    OocJpg,
    /// An already-extracted preview: `750_6926_NEF_embedded.jpg`
    Derived,
    /// Our metadata: `750_6926.xmp`
    Sidecar,
}

impl Role {
    /// A sidecar describes a shot but is not the shot. A stack exists only if
    /// it has at least one image member, so a stray `.xmp` cannot conjure one.
    pub fn is_image(self) -> bool {
        matches!(self, Role::Raw | Role::OocJpg | Role::Derived)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Raw => "raw",
            Role::OocJpg => "oocjpg",
            Role::Derived => "derived",
            Role::Sidecar => "sidecar",
        }
    }
}

/// Suffixes marking a file as derived from another in the same stack. Trailing
/// position only, applied after the extension is removed.
pub const DERIVED_SUFFIXES: &[&str] = &["_NEF_embedded"];

/// The whitelist. Anything not named here is ignored outright -- not
/// catalogued, not counted, not shown.
///
/// `.pp3` is deliberately absent: those are RawTherapee's processing profiles
/// and none of this tool's business. They still travel with their stack when
/// it is trashed, because leaving a profile behind pointing at a moved image
/// is litter, but nothing else here knows they exist.
const RAW_EXTS: &[&str] = &["nef"];
const JPG_EXTS: &[&str] = &["jpg", "jpeg"];
const SIDECAR_EXTS: &[&str] = &["xmp"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classified {
    /// Stack basename, derived suffix stripped.
    pub base: String,
    pub role: Role,
}

/// Classify one filename. `None` means "not ours" -- ignored entirely.
///
/// Extension matching is case-insensitive, because `.JPG` and `.jpg` both
/// occur here and mean different things by convention, not by case. The stored
/// name always stays literal.
pub fn classify(filename: &str) -> Option<Classified> {
    let lower = filename.to_ascii_lowercase();

    let dot = lower.rfind('.')?;
    let ext = &lower[dot + 1..];
    let stem = &filename[..dot];

    if SIDECAR_EXTS.contains(&ext) {
        return Some(Classified {
            base: stem.to_string(),
            role: Role::Sidecar,
        });
    }

    if RAW_EXTS.contains(&ext) {
        return Some(Classified {
            base: stem.to_string(),
            role: Role::Raw,
        });
    }

    if JPG_EXTS.contains(&ext) {
        let stripped = strip_derived(stem);
        let role = if stripped.len() != stem.len() {
            Role::Derived
        } else {
            Role::OocJpg
        };
        return Some(Classified {
            base: stripped.to_string(),
            role,
        });
    }

    None
}

fn strip_derived(stem: &str) -> &str {
    for suffix in DERIVED_SUFFIXES {
        if stem.len() > suffix.len() && stem.ends_with(suffix) {
            return &stem[..stem.len() - suffix.len()];
        }
    }
    stem
}

/// The sidecar path for a stack: `<dir>/<base>.xmp`, shared by every member.
pub fn sidecar_path(dir: &Path, base: &str) -> std::path::PathBuf {
    dir.join(format!("{base}.xmp"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str) -> (String, Role) {
        let r = classify(name).expect("should classify");
        (r.base, r.role)
    }

    #[test]
    fn classifies_the_four_member_kinds() {
        assert_eq!(c("750_6926.NEF"), ("750_6926".into(), Role::Raw));
        assert_eq!(c("750_6926.JPG"), ("750_6926".into(), Role::OocJpg));
        assert_eq!(
            c("750_6926_NEF_embedded.jpg"),
            ("750_6926".into(), Role::Derived)
        );
        assert_eq!(c("750_6926.xmp"), ("750_6926".into(), Role::Sidecar));
    }

    /// The whole point: a derived preview must not become its own stack.
    #[test]
    fn derived_previews_fold_into_their_parent() {
        let raw = classify("750_6926.NEF").unwrap();
        let derived = classify("750_6926_NEF_embedded.jpg").unwrap();
        assert_eq!(raw.base, derived.base);
        assert_ne!(raw.role, derived.role);
    }

    /// `_1` is part of the name, not a derived marker. Only the configured
    /// suffixes strip, or every underscore-numbered frame would collapse.
    #[test]
    fn only_configured_suffixes_strip() {
        assert_eq!(c("750_5744_1.NEF"), ("750_5744_1".into(), Role::Raw));
        assert_eq!(
            c("750_6262_modified.JPG"),
            ("750_6262_modified".into(), Role::OocJpg)
        );
    }

    /// RawTherapee profiles are not ours. Ignoring them outright is simpler
    /// than modelling them, and it removes the only file type this tool
    /// tracked without ever doing anything useful with.
    #[test]
    fn processing_profiles_are_ignored_entirely() {
        for name in [
            "750_6926.NEF.pp3",
            "750_5770_modified.JPG.pp3",
            "750_6108_NEF_embedded_modified.jpg.pp3",
        ] {
            assert!(classify(name).is_none(), "{name} should be ignored");
        }
    }

    /// The whitelist is closed: anything not named in it is invisible.
    #[test]
    fn only_whitelisted_extensions_are_seen() {
        for name in ["a.NEF", "a.JPG", "a.jpg", "a.jpeg", "a.xmp"] {
            assert!(classify(name).is_some(), "{name} should be catalogued");
        }
        for name in [
            "a.pp3",
            "a.CR2",
            "a.ARW",
            "a.dng",
            "a.tif",
            "a.png",
            "a.mov",
            "a.txt",
            "a.xml",
            "a.json",
            "Thumbs.db",
        ] {
            assert!(classify(name).is_none(), "{name} should be ignored");
        }
    }

    #[test]
    fn extension_case_does_not_matter_but_name_case_is_preserved() {
        assert_eq!(c("Shot.nef").0, "Shot");
        assert_eq!(c("Shot.NEF").0, "Shot");
        assert_eq!(c("Shot.JpEg").1, Role::OocJpg);
    }

    #[test]
    fn ignores_everything_else() {
        assert!(classify("notes.txt").is_none());
        assert!(classify("README").is_none());
        assert!(classify(".hidden").is_none());
        assert!(classify("movie.MOV").is_none());
    }

    #[test]
    fn only_image_members_can_anchor_a_stack() {
        assert!(Role::Raw.is_image());
        assert!(Role::OocJpg.is_image());
        assert!(Role::Derived.is_image());
        assert!(!Role::Sidecar.is_image());
    }
}
