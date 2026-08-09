//! Trash and restore.
//!
//! This is the only part of the program that relocates an original, so it is
//! built to be undoable. Trashing renames a stack into `<library>/.trash/`,
//! preserving its path below the library root -- a rename within one
//! filesystem, which is atomic and instant and cannot half-happen. Nothing is
//! unlinked until `empty`, which counts and confirms first.
//!
//! Every member moves together: RAW, JPG, extracted preview, RawTherapee
//! profile and sidecar. A `.pp3` separated from the RAW it describes is
//! useless, and a sidecar left behind would re-appear as an orphan.

use crate::config::Config;
use crate::db::{now_secs, Catalog};
use anyhow::{bail, Context, Result};
use rusqlite::params;
use std::path::{Path, PathBuf};

pub const TRASH_DIR: &str = ".trash";

#[derive(Debug, Default)]
pub struct TrashStats {
    pub stacks: usize,
    pub files: usize,
    pub errors: Vec<String>,
}

fn trash_root(library: &Path) -> PathBuf {
    library.join(TRASH_DIR)
}

/// Every file belonging to a stack, whatever its role, plus its sidecar.
fn members_on_disk(cat: &Catalog, stack_id: i64) -> Result<(PathBuf, String, Vec<PathBuf>)> {
    let (dir, base): (String, String) = cat.conn.query_row(
        "SELECT dir, base FROM stack WHERE id = ?1",
        params![stack_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let dir = PathBuf::from(dir);

    let mut names: Vec<String> = cat
        .conn
        .prepare("SELECT name FROM file WHERE stack_id = ?1")?
        .query_map(params![stack_id], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    // The sidecar is not a `file` row, so add it explicitly if present.
    let sidecar = format!("{base}.xmp");
    if dir.join(&sidecar).exists() {
        names.push(sidecar);
    }

    let paths = names.into_iter().map(|n| dir.join(n)).collect();
    Ok((dir, base, paths))
}

/// Move stacks into the library's trash. Reversible with [`restore`].
pub fn trash(
    cat: &mut Catalog,
    library: &Path,
    stack_ids: &[i64],
    config: &Config,
) -> Result<TrashStats> {
    let mut stats = TrashStats::default();

    // Refuse the whole batch if any target is protected -- consistent with
    // `edit::apply`, and for the same reason: no half-completed operations.
    for &sid in stack_ids {
        let (dir, _, _) = members_on_disk(cat, sid)?;
        config.ensure_mutable(&dir)?;
    }

    for &sid in stack_ids {
        let (dir, base, paths) = members_on_disk(cat, sid)?;

        let relative = dir.strip_prefix(library).unwrap_or(Path::new(""));
        let target_dir = trash_root(library).join(relative);
        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            stats.errors.push(format!("{}: {e}", target_dir.display()));
            continue;
        }

        let mut moved = Vec::new();
        let mut failed = None;
        for src in &paths {
            let name = src.file_name().unwrap_or_default();
            let dst = target_dir.join(name);
            match std::fs::rename(src, &dst) {
                Ok(()) => moved.push((src.clone(), dst)),
                Err(e) => {
                    failed = Some(format!("{}: {e}", src.display()));
                    break;
                }
            }
        }

        if let Some(err) = failed {
            // Put back whatever already moved: a stack split across the trash
            // boundary is the one state worse than not trashing it at all.
            for (orig, now) in moved.iter().rev() {
                let _ = std::fs::rename(now, orig);
            }
            stats.errors.push(err);
            continue;
        }

        cat.conn.execute(
            "INSERT OR REPLACE INTO trashed (stack_id, orig_dir, trashed_at) VALUES (?1,?2,?3)",
            params![sid, dir.to_string_lossy(), now_secs()],
        )?;
        cat.conn.execute(
            "UPDATE stack SET dir = ?2 WHERE id = ?1",
            params![sid, target_dir.to_string_lossy()],
        )?;

        stats.stacks += 1;
        stats.files += moved.len();
        let _ = base;
    }

    Ok(stats)
}

/// Move a trashed stack back where it came from.
pub fn restore(cat: &mut Catalog, stack_id: i64, config: &Config) -> Result<usize> {
    let orig_dir: String = cat
        .conn
        .query_row(
            "SELECT orig_dir FROM trashed WHERE stack_id = ?1",
            params![stack_id],
            |r| r.get(0),
        )
        .context("that stack is not in the trash")?;
    let orig_dir = PathBuf::from(orig_dir);
    config.ensure_mutable(&orig_dir)?;

    let (current_dir, _, paths) = members_on_disk(cat, stack_id)?;
    std::fs::create_dir_all(&orig_dir)?;

    let mut moved = Vec::new();
    for src in &paths {
        let dst = orig_dir.join(src.file_name().unwrap_or_default());
        if dst.exists() {
            for (orig, now) in moved.iter().rev() {
                let _: std::io::Result<()> = std::fs::rename(now, orig);
            }
            bail!(
                "{} already exists; not overwriting it",
                dst.display()
            );
        }
        std::fs::rename(src, &dst)?;
        moved.push((src.clone(), dst));
    }

    cat.conn.execute(
        "UPDATE stack SET dir = ?2 WHERE id = ?1",
        params![stack_id, orig_dir.to_string_lossy()],
    )?;
    cat.conn
        .execute("DELETE FROM trashed WHERE stack_id = ?1", params![stack_id])?;

    let _ = current_dir;
    Ok(moved.len())
}

#[derive(Debug)]
pub struct TrashedEntry {
    pub stack_id: i64,
    pub base: String,
    pub orig_dir: String,
    pub trashed_at: i64,
}

pub fn list(cat: &Catalog) -> Result<Vec<TrashedEntry>> {
    let mut stmt = cat.conn.prepare(
        "SELECT t.stack_id, s.base, t.orig_dir, t.trashed_at
         FROM trashed t JOIN stack s ON s.id = t.stack_id
         ORDER BY t.trashed_at DESC, s.base",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TrashedEntry {
            stack_id: r.get(0)?,
            base: r.get(1)?,
            orig_dir: r.get(2)?,
            trashed_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The only operation in this program that deletes a photograph. Callers must
/// have confirmed with the user first.
pub fn empty(cat: &mut Catalog, config: &Config) -> Result<TrashStats> {
    let mut stats = TrashStats::default();
    let entries = list(cat)?;

    for e in entries {
        let (dir, _, paths) = members_on_disk(cat, e.stack_id)?;
        config.ensure_mutable(&dir)?;

        for p in &paths {
            match std::fs::remove_file(p) {
                Ok(()) => stats.files += 1,
                Err(err) => stats.errors.push(format!("{}: {err}", p.display())),
            }
        }
        cat.conn
            .execute("DELETE FROM stack WHERE id = ?1", params![e.stack_id])?;
        stats.stacks += 1;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit;

    /// Build a library with one three-member stack plus a sidecar.
    fn library() -> (tempfile::TempDir, Catalog, i64) {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        for name in [
            "750_0001.NEF",
            "750_0001.JPG",
            "750_0001_NEF_embedded.jpg",
            "750_0001.NEF.pp3",
        ] {
            std::fs::write(day.join(name), name.as_bytes()).unwrap();
        }

        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();
        let sid = cat
            .stack_id(day.to_str().unwrap(), "750_0001")
            .unwrap()
            .expect("stack should exist");

        // Give it a tag so we can prove metadata survives the round trip.
        edit::apply(
            &mut cat,
            &[sid],
            &[edit::Change::AddTags(vec!["beach".into()])],
            &Config::default(),
        )
        .unwrap();
        edit::flush(&mut cat, &Config::default()).unwrap();

        (dir, cat, sid)
    }

    #[test]
    fn trashing_moves_every_member_including_profile_and_sidecar() {
        let (dir, mut cat, sid) = library();
        let day = dir.path().join("2026/08/09");

        let stats = trash(&mut cat, dir.path(), &[sid], &Config::default()).unwrap();
        assert!(stats.errors.is_empty(), "{:?}", stats.errors);
        assert_eq!(stats.stacks, 1);
        assert_eq!(stats.files, 5, "raw+jpg+derived+pp3+xmp should all move");

        // Original location emptied of this stack..
        for name in [
            "750_0001.NEF",
            "750_0001.JPG",
            "750_0001_NEF_embedded.jpg",
            "750_0001.NEF.pp3",
            "750_0001.xmp",
        ] {
            assert!(!day.join(name).exists(), "{name} was left behind");
            assert!(
                dir.path().join(".trash/2026/08/09").join(name).exists(),
                "{name} did not arrive in the trash"
            );
        }
    }

    #[test]
    fn restore_puts_everything_back_with_its_metadata() {
        let (dir, mut cat, sid) = library();
        let day = dir.path().join("2026/08/09");

        trash(&mut cat, dir.path(), &[sid], &Config::default()).unwrap();
        let n = restore(&mut cat, sid, &Config::default()).unwrap();
        assert_eq!(n, 5);

        assert!(day.join("750_0001.NEF").exists());
        assert!(day.join("750_0001.xmp").exists());

        // The tag survived, and the trash record is gone.
        let doc = edit::build_doc(&cat, sid).unwrap();
        assert_eq!(doc.tags, vec!["beach"]);
        assert!(list(&cat).unwrap().is_empty());
    }

    /// Trashed stacks must not be picked up again by a scan, or they would
    /// resurrect themselves at a path inside `.trash`.
    #[test]
    fn a_rescan_ignores_the_trash() {
        let (dir, mut cat, sid) = library();
        trash(&mut cat, dir.path(), &[sid], &Config::default()).unwrap();

        let mut fresh = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut fresh, dir.path(), true).unwrap();
        assert_eq!(
            fresh.stack_count().unwrap(),
            0,
            "the scanner walked into .trash"
        );
    }

    #[test]
    fn restore_refuses_rather_than_overwriting_something_in_the_way() {
        let (dir, mut cat, sid) = library();
        let day = dir.path().join("2026/08/09");

        trash(&mut cat, dir.path(), &[sid], &Config::default()).unwrap();
        // Something new takes the old name.
        std::fs::write(day.join("750_0001.NEF"), b"a different shot").unwrap();

        let err = restore(&mut cat, sid, &Config::default()).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read(day.join("750_0001.NEF")).unwrap(),
            b"a different shot",
            "the intruding file was overwritten"
        );
    }

    #[test]
    fn empty_deletes_and_is_the_only_thing_that_does() {
        let (dir, mut cat, sid) = library();
        trash(&mut cat, dir.path(), &[sid], &Config::default()).unwrap();

        // Still on disk after trashing -- nothing was unlinked.
        assert!(dir.path().join(".trash/2026/08/09/750_0001.NEF").exists());

        let stats = empty(&mut cat, &Config::default()).unwrap();
        assert_eq!(stats.stacks, 1);
        assert_eq!(stats.files, 5);
        assert!(!dir.path().join(".trash/2026/08/09/750_0001.NEF").exists());
        assert_eq!(cat.stack_count().unwrap(), 0);
    }

    #[test]
    fn a_protected_library_cannot_be_trashed() {
        let (dir, mut cat, sid) = library();
        let config = Config {
            protected_roots: vec![dir.path().to_path_buf()],
        };
        assert!(trash(&mut cat, dir.path(), &[sid], &config).is_err());
        assert!(
            dir.path().join("2026/08/09/750_0001.NEF").exists(),
            "a protected stack was moved"
        );
    }
}
