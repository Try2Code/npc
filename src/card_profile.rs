//! What `npc import` has already confirmed is in the library, per source
//! volume -- so a card that is never reformatted doesn't cost a full Exiv2
//! read of every old shot on it, every single import.
//!
//! The expensive part of planning an import is not walking the card or
//! `stat()`-ing its files (milliseconds, measured) but the one Exiv2 read per
//! stack that identity-based dedup needs (seconds, serialised behind the
//! Exiv2 mutex). A file whose path, size *and* mtime all match what this
//! profile recorded after an earlier import is the same file, already in the
//! library -- the same `(size, mtime)` trust `scan.rs` already places in a
//! library file it has seen before. A filename alone is not enough: camera
//! numbering recycles.
//!
//! A miss is always safe: anything not matched, including a file whose
//! metadata some other tool rewrote on the card (its size or mtime moves),
//! simply goes through the full check as before. `npc import --deep` ignores
//! the profile entirely.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub struct CardProfile {
    file: PathBuf,
    mount: PathBuf,
    /// Path relative to the volume's mount point -> (size, mtime in ns).
    /// Relative to the mount, not to whatever source directory was given,
    /// so importing a card's root once and its `DCIM` folder the next time
    /// still agree.
    known: HashMap<PathBuf, (u64, i64)>,
}

impl CardProfile {
    /// The profile for `vol`, kept under `store/cards/<uuid>.tsv`. `None` for
    /// a volume with no stable identity: a device-number fallback can name a
    /// different card after a replug, and trusting its profile would skip
    /// files that were never imported.
    pub fn open(store: &Path, vol: &crate::vol::Volume) -> Result<Option<Self>> {
        if !vol.stable {
            return Ok(None);
        }
        let file = store.join("cards").join(format!("{}.tsv", vol.uuid));
        let mount = vol
            .mount
            .canonicalize()
            .with_context(|| format!("resolving {}", vol.mount.display()))?;
        let known = match std::fs::read_to_string(&file) {
            Ok(text) => parse(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", file.display())),
        };
        Ok(Some(Self { file, mount, known }))
    }

    /// Whether `path` is exactly the file recorded earlier: same place on
    /// the volume, same size, same mtime.
    pub fn contains(&self, path: &Path) -> bool {
        self.key(path)
            .is_some_and(|(rel, size, mtime)| self.known.get(&rel) == Some(&(size, mtime)))
    }

    /// Remember `path` as confirmed in the library, as it is right now.
    pub fn record(&mut self, path: &Path) {
        if let Some((rel, size, mtime)) = self.key(path) {
            self.known.insert(rel, (size, mtime));
        }
    }

    pub fn len(&self) -> usize {
        self.known.len()
    }

    pub fn is_empty(&self) -> bool {
        self.known.is_empty()
    }

    /// Written to a temporary name and renamed into place, so an interrupted
    /// save leaves the previous profile rather than half of one.
    pub fn save(&self) -> Result<()> {
        let dir = self.file.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;

        let mut lines: Vec<String> = self
            .known
            .iter()
            .filter_map(|(rel, (size, mtime))| {
                // A newline would split the record; such a name can't come
                // from a camera, and leaving it out only costs a full check.
                let rel = rel.to_str().filter(|r| !r.contains('\n'))?;
                Some(format!("{size}\t{mtime}\t{rel}"))
            })
            .collect();
        lines.sort();
        let mut body = lines.join("\n");
        body.push('\n');

        let tmp = self.file.with_extension("tsv.part");
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.file)
            .with_context(|| format!("renaming into {}", self.file.display()))
    }

    fn key(&self, path: &Path) -> Option<(PathBuf, u64, i64)> {
        let canon = path.canonicalize().ok()?;
        let rel = canon.strip_prefix(&self.mount).ok()?.to_path_buf();
        let m = std::fs::metadata(&canon).ok()?;
        Some((rel, m.len(), m.mtime() * 1_000_000_000 + m.mtime_nsec()))
    }
}

fn parse(text: &str) -> HashMap<PathBuf, (u64, i64)> {
    text.lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, '\t');
            let size = parts.next()?.parse().ok()?;
            let mtime = parts.next()?.parse().ok()?;
            let rel = parts.next()?;
            Some((PathBuf::from(rel), (size, mtime)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vol::Volume;

    fn volume(mount: &Path, stable: bool) -> Volume {
        Volume {
            uuid: "TEST-CARD".into(),
            label: None,
            mount: mount.to_path_buf(),
            device: None,
            stable,
        }
    }

    #[test]
    fn a_recorded_file_is_known_after_a_save_and_reopen() {
        let card = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let shot = card.path().join("DCIM/100ND750/750_0001.NEF");
        std::fs::create_dir_all(shot.parent().unwrap()).unwrap();
        std::fs::write(&shot, "raw").unwrap();

        let vol = volume(card.path(), true);
        let mut p = CardProfile::open(store.path(), &vol).unwrap().unwrap();
        assert!(!p.contains(&shot));
        p.record(&shot);
        p.save().unwrap();

        let p = CardProfile::open(store.path(), &vol).unwrap().unwrap();
        assert!(p.contains(&shot));
        assert_eq!(p.len(), 1);
    }

    /// The whole point of keying on more than the name: a recycled number
    /// with different content must not be mistaken for the old shot.
    #[test]
    fn the_same_name_with_a_different_size_is_not_known() {
        let card = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let shot = card.path().join("750_0001.NEF");
        std::fs::write(&shot, "raw").unwrap();

        let mut p = CardProfile::open(store.path(), &volume(card.path(), true))
            .unwrap()
            .unwrap();
        p.record(&shot);
        std::fs::write(&shot, "a different, longer raw").unwrap();
        assert!(!p.contains(&shot));
    }

    #[test]
    fn the_same_name_and_size_with_a_different_mtime_is_not_known() {
        let card = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let shot = card.path().join("750_0001.NEF");
        std::fs::write(&shot, "raw").unwrap();

        let mut p = CardProfile::open(store.path(), &volume(card.path(), true))
            .unwrap()
            .unwrap();
        p.record(&shot);
        let f = std::fs::File::options().write(true).open(&shot).unwrap();
        f.set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        assert!(!p.contains(&shot));
    }

    /// Keyed relative to the mount, so the card's root and its `DCIM` folder
    /// given as the source on different runs still agree.
    #[test]
    fn keys_are_relative_to_the_mount_not_the_source_given() {
        let card = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let shot = card.path().join("DCIM/750_0001.NEF");
        std::fs::create_dir_all(shot.parent().unwrap()).unwrap();
        std::fs::write(&shot, "raw").unwrap();

        let mut p = CardProfile::open(store.path(), &volume(card.path(), true))
            .unwrap()
            .unwrap();
        p.record(&card.path().join("DCIM/../DCIM/750_0001.NEF"));
        assert!(p.contains(&shot));
    }

    #[test]
    fn an_unstable_volume_gets_no_profile() {
        let card = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        assert!(CardProfile::open(store.path(), &volume(card.path(), false))
            .unwrap()
            .is_none());
    }
}
