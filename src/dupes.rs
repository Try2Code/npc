//! Finding the same shot twice.
//!
//! Two passes, strongest evidence first:
//!
//! 1. **Body serial plus shutter count.** A Nikon writes both into the maker
//!    note, and the pair is unique to one actuation of one camera. Two files
//!    carrying the same pair are the same photograph, whatever they are named.
//! 2. **Content hash**, for anything lacking that pair -- JPGs stripped of
//!    maker notes, other makes, exported copies. Only files that already agree
//!    on `(capture_time, size)` are hashed, so a large library is never hashed
//!    wholesale.
//!
//! Nothing is deleted here. Groups are reported and left to `trash`.

use crate::db::Catalog;
use crate::import::hash_file;
use anyhow::Result;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct DupeGroup {
    pub reason: String,
    /// `dir/name` for each file in the group, oldest cataloged first.
    pub paths: Vec<String>,
}

pub fn find(cat: &Catalog) -> Result<Vec<DupeGroup>> {
    let mut groups = Vec::new();
    groups.extend(by_actuation(cat)?);
    groups.extend(by_content(cat)?);
    Ok(groups)
}

/// Same camera body, same shutter actuation.
fn by_actuation(cat: &Catalog) -> Result<Vec<DupeGroup>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.serial, f.shutter_count, s.dir || '/' || f.name
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.present = 1
           AND f.serial IS NOT NULL AND f.serial != ''
           AND f.shutter_count IS NOT NULL AND f.shutter_count != 0
           AND f.role = 'raw'
         ORDER BY f.id",
    )?;

    let mut buckets: HashMap<(String, i64), Vec<String>> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (serial, count, path) = row?;
        buckets.entry((serial, count)).or_default().push(path);
    }

    let mut out: Vec<DupeGroup> = buckets
        .into_iter()
        .filter(|(_, paths)| paths.len() > 1)
        .map(|((serial, count), paths)| DupeGroup {
            reason: format!("same body {serial}, shutter actuation {count}"),
            paths,
        })
        .collect();
    out.sort_by(|a, b| a.paths.cmp(&b.paths));
    Ok(out)
}

/// Identical bytes, among files that already agree on capture time and size.
fn by_content(cat: &Catalog) -> Result<Vec<DupeGroup>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.capture_time, f.size, s.dir || '/' || f.name
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.present = 1 AND f.role IN ('raw','oocjpg')
           AND (f.serial IS NULL OR f.serial = '' OR f.shutter_count IS NULL OR f.shutter_count = 0)
           AND f.capture_time IS NOT NULL AND f.size IS NOT NULL
         ORDER BY f.id",
    )?;

    let mut candidates: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (t, size, path) = row?;
        candidates.entry((t, size)).or_default().push(path);
    }

    let mut out = Vec::new();
    for (_, paths) in candidates.into_iter().filter(|(_, p)| p.len() > 1) {
        // Only now does anything get read off disk.
        let mut by_hash: HashMap<String, Vec<String>> = HashMap::new();
        for p in paths {
            if let Ok(h) = hash_file(Path::new(&p)) {
                by_hash.entry(h.to_hex().to_string()).or_default().push(p);
            }
        }
        for (hash, group) in by_hash {
            if group.len() > 1 {
                out.push(DupeGroup {
                    reason: format!("identical content ({}…)", &hash[..12]),
                    paths: group,
                });
            }
        }
    }
    out.sort_by(|a, b| a.paths.cmp(&b.paths));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert(cat: &Catalog, dir: &str, base: &str, serial: &str, shutter: i64) {
        cat.conn
            .execute(
                "INSERT OR IGNORE INTO stack (dir, base) VALUES (?1, ?2)",
                rusqlite::params![dir, base],
            )
            .unwrap();
        let sid = cat.stack_id(dir, base).unwrap().unwrap();
        cat.conn
            .execute(
                "INSERT INTO file (stack_id, name, role, serial, shutter_count, present)
                 VALUES (?1, ?2, 'raw', ?3, ?4, 1)",
                rusqlite::params![sid, format!("{base}.NEF"), serial, shutter],
            )
            .unwrap();
    }

    #[test]
    fn the_same_actuation_in_two_places_is_a_duplicate() {
        let cat = Catalog::open_memory().unwrap();
        insert(&cat, "/lib/2026/08/09", "750_0001", "6094708", 63987);
        insert(&cat, "/lib/backup", "copy_of_0001", "6094708", 63987);
        insert(&cat, "/lib/2026/08/09", "750_0002", "6094708", 63988);

        let groups = find(&cat).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].paths.len(), 2);
        assert!(groups[0].reason.contains("63987"));
    }

    /// The same shutter count from a *different body* is not a duplicate --
    /// counts collide across cameras constantly.
    #[test]
    fn the_same_count_on_a_different_body_is_not_a_duplicate() {
        let cat = Catalog::open_memory().unwrap();
        insert(&cat, "/lib/a", "shot", "6094708", 1000);
        insert(&cat, "/lib/b", "shot", "9999999", 1000);
        assert!(find(&cat).unwrap().is_empty());
    }

    #[test]
    fn distinct_shots_are_not_grouped() {
        let cat = Catalog::open_memory().unwrap();
        insert(&cat, "/lib", "a", "6094708", 1);
        insert(&cat, "/lib", "b", "6094708", 2);
        insert(&cat, "/lib", "c", "6094708", 3);
        assert!(find(&cat).unwrap().is_empty());
    }

    #[test]
    fn identical_bytes_are_caught_when_maker_notes_are_missing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.JPG");
        let b = dir.path().join("b.JPG");
        std::fs::write(&a, b"the same photograph").unwrap();
        std::fs::write(&b, b"the same photograph").unwrap();

        let cat = Catalog::open_memory().unwrap();
        for (name, path) in [("a", &a), ("b", &b)] {
            cat.conn
                .execute(
                    "INSERT INTO stack (dir, base) VALUES (?1, ?2)",
                    rusqlite::params![dir.path().to_str().unwrap(), name],
                )
                .unwrap();
            let sid = cat.conn.last_insert_rowid();
            cat.conn
                .execute(
                    "INSERT INTO file (stack_id, name, role, capture_time, size, present)
                     VALUES (?1, ?2, 'oocjpg', 1786233666, ?3, 1)",
                    rusqlite::params![
                        sid,
                        path.file_name().unwrap().to_str().unwrap(),
                        i64::try_from(std::fs::metadata(path).unwrap().len()).unwrap()
                    ],
                )
                .unwrap();
        }

        let groups = find(&cat).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(groups[0].reason.contains("identical content"));
    }

    /// Same size and capture time but different bytes must not be reported --
    /// that is the whole reason the hash pass exists.
    #[test]
    fn same_size_but_different_bytes_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.JPG"), b"photograph one!!").unwrap();
        std::fs::write(dir.path().join("b.JPG"), b"photograph two!!").unwrap();

        let cat = Catalog::open_memory().unwrap();
        for name in ["a", "b"] {
            cat.conn
                .execute(
                    "INSERT INTO stack (dir, base) VALUES (?1, ?2)",
                    rusqlite::params![dir.path().to_str().unwrap(), name],
                )
                .unwrap();
            let sid = cat.conn.last_insert_rowid();
            cat.conn
                .execute(
                    "INSERT INTO file (stack_id, name, role, capture_time, size, present)
                     VALUES (?1, ?2, 'oocjpg', 1786233666, 16, 1)",
                    rusqlite::params![sid, format!("{name}.JPG")],
                )
                .unwrap();
        }

        assert!(find(&cat).unwrap().is_empty());
    }
}
