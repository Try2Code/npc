//! Changing metadata: apply to the catalog, then flush to sidecars.
//!
//! Edits land in the catalog immediately and mark the stack dirty. Flushing
//! writes `<base>.xmp` beside the shot. The two steps are separate so that
//! edits made with the drive unplugged simply stay dirty until it comes back --
//! the `pending` mechanism is the same code path as a normal flush.
//!
//! Nothing here can reach an original image: the sidecar path is derived from
//! the stack, the shim refuses anything not ending in `.xmp`, and protected
//! roots are checked before either.

use crate::config::Config;
use crate::db::{now_secs, Catalog};
use crate::meta::{self, SidecarDoc};
use anyhow::{Context, Result};
use rusqlite::params;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub enum Change {
    AddTags(Vec<String>),
    RemoveTags(Vec<String>),
    ClearTags,
    SetRating(Option<i64>),
    SetFlag(bool),
    SetReject(bool),
    SetTitle(String),
    SetDescription(String),
    SetOrientation(u16),
}

#[derive(Debug, Default, Clone)]
pub struct EditStats {
    pub stacks_changed: usize,
    pub sidecars_written: usize,
    pub queued_offline: usize,
    pub errors: Vec<String>,
}

/// Apply changes to the given stacks and mark them dirty. Does not touch disk.
///
/// Protected roots are checked **here**, before anything is mutated, and the
/// whole call fails if any target is protected. Checking only at flush time
/// would leave the catalog claiming tags that were never written to disk --
/// a silent divergence between the index and the source of truth.
pub fn apply(
    cat: &mut Catalog,
    stack_ids: &[i64],
    changes: &[Change],
    config: &Config,
) -> Result<usize> {
    let mut blocked = Vec::new();
    for &sid in stack_ids {
        let dir: String = cat
            .conn
            .query_row("SELECT dir FROM stack WHERE id = ?1", params![sid], |r| {
                r.get(0)
            })
            .with_context(|| format!("no stack {sid}"))?;
        if config.ensure_mutable(Path::new(&dir)).is_err() {
            blocked.push(dir);
        }
    }
    if !blocked.is_empty() {
        blocked.sort();
        blocked.dedup();
        anyhow::bail!(
            "refusing to edit {} stack(s): {} {} inside a protected root.\n\
             Nothing was changed. Protected roots live in {}.",
            stack_ids.len(),
            blocked.first().map(String::as_str).unwrap_or(""),
            if blocked.len() > 1 {
                format!("and {} other directories are", blocked.len() - 1)
            } else {
                "is".to_string()
            },
            Config::path().display()
        );
    }

    let tx = cat.conn.transaction()?;
    {
        for &sid in stack_ids {
            for change in changes {
                match change {
                    Change::AddTags(tags) => {
                        for t in tags {
                            let t = normalise_tag(t);
                            if t.is_empty() {
                                continue;
                            }
                            tx.execute("INSERT OR IGNORE INTO tag (name) VALUES (?1)", params![t])?;
                            let tid: i64 = tx.query_row(
                                "SELECT id FROM tag WHERE name = ?1",
                                params![t],
                                |r| r.get(0),
                            )?;
                            tx.execute(
                                "INSERT OR IGNORE INTO stack_tag (stack_id, tag_id) VALUES (?1,?2)",
                                params![sid, tid],
                            )?;
                        }
                    }
                    Change::RemoveTags(tags) => {
                        for t in tags {
                            let t = normalise_tag(t);
                            tx.execute(
                                "DELETE FROM stack_tag WHERE stack_id = ?1 AND tag_id IN
                                 (SELECT id FROM tag WHERE name = ?2)",
                                params![sid, t],
                            )?;
                        }
                    }
                    Change::ClearTags => {
                        tx.execute("DELETE FROM stack_tag WHERE stack_id = ?1", params![sid])?;
                    }
                    Change::SetRating(r) => {
                        tx.execute(
                            "UPDATE stack SET rating = ?2 WHERE id = ?1",
                            params![sid, r],
                        )?;
                    }
                    Change::SetFlag(v) => {
                        tx.execute(
                            "UPDATE stack SET flag = ?2, reject = CASE WHEN ?2 = 1 THEN 0 ELSE reject END
                             WHERE id = ?1",
                            params![sid, *v as i64],
                        )?;
                    }
                    Change::SetReject(v) => {
                        tx.execute(
                            "UPDATE stack SET reject = ?2, flag = CASE WHEN ?2 = 1 THEN 0 ELSE flag END
                             WHERE id = ?1",
                            params![sid, *v as i64],
                        )?;
                    }
                    Change::SetTitle(t) => {
                        tx.execute("UPDATE stack SET title = ?2 WHERE id = ?1", params![sid, t])?;
                    }
                    Change::SetDescription(t) => {
                        tx.execute(
                            "UPDATE stack SET description = ?2 WHERE id = ?1",
                            params![sid, t],
                        )?;
                    }
                    Change::SetOrientation(o) => {
                        tx.execute(
                            "UPDATE stack SET orientation = ?2 WHERE id = ?1",
                            params![sid, *o as i64],
                        )?;
                    }
                }
            }
            tx.execute("UPDATE stack SET dirty = 1 WHERE id = ?1", params![sid])?;
            tx.execute(
                "INSERT OR REPLACE INTO pending (stack_id, queued_at) VALUES (?1, ?2)",
                params![sid, now_secs()],
            )?;
        }
    }
    tx.commit()?;
    Ok(stack_ids.len())
}

/// Write every dirty stack's sidecar. Stacks whose directory is missing (drive
/// unplugged) stay queued rather than erroring.
pub fn flush(cat: &mut Catalog, config: &Config) -> Result<EditStats> {
    let mut stats = EditStats::default();

    let dirty: Vec<(i64, String, String)> = {
        let mut stmt = cat
            .conn
            .prepare("SELECT id, dir, base FROM stack WHERE dirty = 1 ORDER BY dir, base")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };

    for (sid, dir, base) in dirty {
        let sidecar = PathBuf::from(&dir).join(format!("{base}.xmp"));

        if let Err(e) = config.ensure_mutable(&sidecar) {
            stats.errors.push(e.to_string());
            continue;
        }

        if !Path::new(&dir).is_dir() {
            // Offline: leave it dirty and queued for the next flush.
            stats.queued_offline += 1;
            continue;
        }

        let doc = build_doc(cat, sid)?;
        match meta::write_sidecar(sidecar.to_str().unwrap_or_default(), &doc) {
            Ok(()) => {
                let mtime = std::fs::metadata(&sidecar)
                    .ok()
                    .and_then(|m| {
                        use std::os::unix::fs::MetadataExt;
                        Some(m.mtime() * 1_000_000_000 + m.mtime_nsec())
                    })
                    .unwrap_or(0);
                cat.conn.execute(
                    "UPDATE stack SET dirty = 0, sidecar_mtime = ?2 WHERE id = ?1",
                    params![sid, mtime],
                )?;
                cat.conn
                    .execute("DELETE FROM pending WHERE stack_id = ?1", params![sid])?;
                stats.sidecars_written += 1;
            }
            Err(e) => stats.errors.push(format!("{}: {e}", sidecar.display())),
        }
    }

    Ok(stats)
}

/// Assemble the sidecar contents from the catalog's view of a stack.
pub fn build_doc(cat: &Catalog, stack_id: i64) -> Result<SidecarDoc> {
    let (rating, flag, reject, title, description, orientation): (
        Option<i64>,
        i64,
        i64,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = cat.conn.query_row(
        "SELECT rating, flag, reject, title, description, orientation FROM stack WHERE id = ?1",
        params![stack_id],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;

    let tags: Vec<String> = cat
        .conn
        .prepare(
            "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
             WHERE st.stack_id = ?1 ORDER BY t.name",
        )?
        .query_map(params![stack_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;

    // Adobe's convention: one Label string carries both states.
    let label = if flag == 1 {
        "Flagged".to_string()
    } else if reject == 1 {
        "Rejected".to_string()
    } else {
        String::new()
    };

    Ok(SidecarDoc {
        exists: true,
        tags,
        rating: rating.unwrap_or(-1) as i32,
        label,
        title: title.unwrap_or_default(),
        description: description.unwrap_or_default(),
        orientation: orientation.unwrap_or(0) as u16,
    })
}

/// Read a sidecar into the catalog. Used by `scan` when a sidecar is new or
/// its mtime moved, so tags made in another application are picked up.
pub fn ingest_sidecar(cat: &mut Catalog, stack_id: i64, sidecar: &Path) -> Result<bool> {
    let doc = meta::read_sidecar(sidecar.to_str().unwrap_or_default())
        .with_context(|| format!("reading {}", sidecar.display()))?;
    if !doc.exists {
        return Ok(false);
    }

    let (flag, reject) = match doc.label.as_str() {
        "Flagged" => (1, 0),
        "Rejected" => (0, 1),
        _ => (0, 0),
    };

    let tx = cat.conn.transaction()?;
    tx.execute(
        "UPDATE stack SET rating = ?2, flag = ?3, reject = ?4, title = ?5,
                          description = ?6, orientation = ?7
         WHERE id = ?1",
        params![
            stack_id,
            if doc.rating >= 0 { Some(doc.rating as i64) } else { None },
            flag,
            reject,
            (!doc.title.is_empty()).then_some(doc.title.clone()),
            (!doc.description.is_empty()).then_some(doc.description.clone()),
            (doc.orientation > 0).then_some(doc.orientation as i64),
        ],
    )?;
    tx.execute("DELETE FROM stack_tag WHERE stack_id = ?1", params![stack_id])?;
    for t in &doc.tags {
        let t = normalise_tag(t);
        if t.is_empty() {
            continue;
        }
        tx.execute("INSERT OR IGNORE INTO tag (name) VALUES (?1)", params![t])?;
        let tid: i64 = tx.query_row("SELECT id FROM tag WHERE name = ?1", params![t], |r| {
            r.get(0)
        })?;
        tx.execute(
            "INSERT OR IGNORE INTO stack_tag (stack_id, tag_id) VALUES (?1,?2)",
            params![stack_id, tid],
        )?;
    }
    tx.commit()?;
    Ok(true)
}

/// Trim, collapse inner whitespace, drop leading/trailing slashes so `a/b`
/// hierarchies stay well-formed.
fn normalise_tag(t: &str) -> String {
    t.trim()
        .trim_matches('/')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded() -> (Catalog, i64) {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute("INSERT INTO stack (dir, base) VALUES ('/d', '750_0001')", [])
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        (cat, sid)
    }

    #[test]
    fn tags_are_normalised() {
        assert_eq!(normalise_tag("  beach  "), "beach");
        assert_eq!(normalise_tag("/beach/"), "beach");
        assert_eq!(normalise_tag("family   trip"), "family trip");
        assert_eq!(normalise_tag("   "), "");
    }

    #[test]
    fn adding_and_removing_tags() {
        let (mut cat, sid) = seeded();
        apply(&mut cat, &[sid], &[Change::AddTags(vec!["beach".into(), "anna".into()])], &Config::default()).unwrap();
        assert_eq!(build_doc(&cat, sid).unwrap().tags, vec!["anna", "beach"]);

        apply(&mut cat, &[sid], &[Change::RemoveTags(vec!["anna".into()])], &Config::default()).unwrap();
        assert_eq!(build_doc(&cat, sid).unwrap().tags, vec!["beach"]);
    }

    #[test]
    fn adding_the_same_tag_twice_is_idempotent() {
        let (mut cat, sid) = seeded();
        apply(&mut cat, &[sid], &[Change::AddTags(vec!["beach".into()])], &Config::default()).unwrap();
        apply(&mut cat, &[sid], &[Change::AddTags(vec!["BEACH".into()])], &Config::default()).unwrap();
        assert_eq!(build_doc(&cat, sid).unwrap().tags.len(), 1);
    }

    /// Flag and reject are mutually exclusive: Adobe stores both in one Label.
    #[test]
    fn flag_and_reject_exclude_each_other() {
        let (mut cat, sid) = seeded();
        apply(&mut cat, &[sid], &[Change::SetFlag(true)], &Config::default()).unwrap();
        assert_eq!(build_doc(&cat, sid).unwrap().label, "Flagged");

        apply(&mut cat, &[sid], &[Change::SetReject(true)], &Config::default()).unwrap();
        let doc = build_doc(&cat, sid).unwrap();
        assert_eq!(doc.label, "Rejected");

        let flag: i64 = cat
            .conn
            .query_row("SELECT flag FROM stack WHERE id = ?1", [sid], |r| r.get(0))
            .unwrap();
        assert_eq!(flag, 0, "setting reject must clear flag");
    }

    #[test]
    fn editing_marks_the_stack_dirty_and_queues_it() {
        let (mut cat, sid) = seeded();
        apply(&mut cat, &[sid], &[Change::SetRating(Some(4))], &Config::default()).unwrap();

        let dirty: i64 = cat
            .conn
            .query_row("SELECT dirty FROM stack WHERE id = ?1", [sid], |r| r.get(0))
            .unwrap();
        let queued: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM pending WHERE stack_id = ?1", [sid], |r| r.get(0))
            .unwrap();
        assert_eq!((dirty, queued), (1, 1));
    }

    /// The drive being absent must not lose an edit or raise an error: the
    /// stack stays dirty and queued for the next flush.
    #[test]
    fn edits_to_a_missing_directory_stay_queued() {
        let (mut cat, sid) = seeded(); // dir '/d' does not exist
        apply(&mut cat, &[sid], &[Change::SetRating(Some(5))], &Config::default()).unwrap();

        let stats = flush(&mut cat, &Config::default()).unwrap();
        assert_eq!(stats.sidecars_written, 0);
        assert_eq!(stats.queued_offline, 1);
        assert!(stats.errors.is_empty());

        let dirty: i64 = cat
            .conn
            .query_row("SELECT dirty FROM stack WHERE id = ?1", [sid], |r| r.get(0))
            .unwrap();
        assert_eq!(dirty, 1, "an offline edit must remain pending");
    }

    #[test]
    fn flush_writes_a_sidecar_and_clears_the_dirty_flag() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, '750_0001')",
                [dir.path().to_str().unwrap()],
            )
            .unwrap();
        let sid = cat.conn.last_insert_rowid();

        apply(&mut cat, &[sid], &[
                Change::AddTags(vec!["beach".into()]),
                Change::SetRating(Some(4)),
            ], &Config::default()).unwrap();

        let stats = flush(&mut cat, &Config::default()).unwrap();
        assert_eq!(stats.sidecars_written, 1, "errors: {:?}", stats.errors);

        let sidecar = dir.path().join("750_0001.xmp");
        assert!(sidecar.exists());

        let dirty: i64 = cat
            .conn
            .query_row("SELECT dirty FROM stack WHERE id = ?1", [sid], |r| r.get(0))
            .unwrap();
        assert_eq!(dirty, 0);
    }

    /// Regression: the guard must refuse **before** mutating the catalog.
    /// Checking only at flush time left the index claiming tags that were
    /// never written to disk, and the command still exited zero.
    #[test]
    fn a_protected_target_is_refused_without_touching_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, '750_0001')",
                [dir.path().to_str().unwrap()],
            )
            .unwrap();
        let sid = cat.conn.last_insert_rowid();

        let config = Config {
            protected_roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };

        let err = apply(
            &mut cat,
            &[sid],
            &[Change::AddTags(vec!["should-never-appear".into()])],
            &config,
        )
        .expect_err("editing inside a protected root must fail");
        assert!(err.to_string().contains("Nothing was changed"));

        // ..and nothing was: no tag, not dirty, not queued.
        let tags: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM stack_tag", [], |r| r.get(0))
            .unwrap();
        let dirty: i64 = cat
            .conn
            .query_row("SELECT dirty FROM stack WHERE id = ?1", [sid], |r| r.get(0))
            .unwrap();
        let queued: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!((tags, dirty, queued), (0, 0, 0));
    }

    /// A protected root must stop a flush even though everything else about
    /// the edit is valid.
    #[test]
    fn flush_refuses_to_write_inside_a_protected_root() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, '750_0001')",
                [dir.path().to_str().unwrap()],
            )
            .unwrap();
        let sid = cat.conn.last_insert_rowid();
        apply(&mut cat, &[sid], &[Change::SetRating(Some(4))], &Config::default()).unwrap();

        let config = Config {
            protected_roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let stats = flush(&mut cat, &config).unwrap();

        assert_eq!(stats.sidecars_written, 0);
        assert_eq!(stats.errors.len(), 1);
        assert!(stats.errors[0].contains("protected"));
        assert!(!dir.path().join("750_0001.xmp").exists());
    }

    /// Round trip through the filesystem: what we write is what we read back.
    #[test]
    fn sidecars_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, '750_0001')",
                [dir.path().to_str().unwrap()],
            )
            .unwrap();
        let sid = cat.conn.last_insert_rowid();

        apply(&mut cat, &[sid], &[
                Change::AddTags(vec!["beach".into(), "anna".into()]),
                Change::SetRating(Some(4)),
                Change::SetFlag(true),
                Change::SetTitle("Ostsee".into()),
                Change::SetDescription("low tide".into()),
            ], &Config::default()).unwrap();
        flush(&mut cat, &Config::default()).unwrap();

        // Wipe the catalog's view, then read it back off disk.
        cat.conn
            .execute("UPDATE stack SET rating=NULL, flag=0, title=NULL, description=NULL", [])
            .unwrap();
        cat.conn.execute("DELETE FROM stack_tag", []).unwrap();

        let sidecar = dir.path().join("750_0001.xmp");
        assert!(ingest_sidecar(&mut cat, sid, &sidecar).unwrap());

        let doc = build_doc(&cat, sid).unwrap();
        assert_eq!(doc.tags, vec!["anna", "beach"]);
        assert_eq!(doc.rating, 4);
        assert_eq!(doc.label, "Flagged");
        assert_eq!(doc.title, "Ostsee");
        assert_eq!(doc.description, "low tide");
    }
}
