//! Walking the library and building the catalog.
//!
//! The performance rule: an incremental rescan costs roughly one `stat()` per
//! file and nothing more. Files whose `(size, mtime_ns)` are unchanged are
//! never opened, which is what keeps Exiv2's per-file cost off the daily path.

use crate::db::{now_secs, Catalog};
use crate::meta;
use crate::stack::{classify, Role};
use anyhow::{Context, Result};
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::Path;
use walkdir::WalkDir;

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub files_seen: usize,
    pub unchanged: usize,
    pub parsed: usize,
    pub stacks: usize,
    pub orphans: usize,
    pub vanished: usize,
    pub sidecars_read: usize,
    pub errors: Vec<String>,
}

struct Member {
    name: String,
    ext: String,
    role: Role,
    size: i64,
    mtime_ns: i64,
    needs_parse: bool,
}

/// Scan `root` into the catalog. Originals are only ever opened read-only, so
/// this cannot alter the drive even if it panics mid-walk.
pub fn scan(cat: &mut Catalog, root: &Path, full: bool) -> Result<ScanStats> {
    let mut stats = ScanStats::default();
    let known = if full {
        HashMap::new()
    } else {
        cat.fingerprints()?
    };

    // --- walk and classify -------------------------------------------------
    // Grouped by (dir, base): the stack is the unit, so members must be seen
    // together before we can tell a real stack from an orphan profile.
    let mut groups: HashMap<(String, String), Vec<Member>> = HashMap::new();
    let mut seen_paths: Vec<String> = Vec::new();

    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // `.trash/` holds deleted stacks and must not re-enter the catalog.
            e.file_name() != ".trash"
        })
    {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                stats.errors.push(format!("walk: {e}"));
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(c) = classify(&name) else { continue };

        stats.files_seen += 1;

        let dir = entry
            .path()
            .parent()
            .unwrap_or(root)
            .to_string_lossy()
            .to_string();
        let full_path = format!("{dir}/{name}");
        seen_paths.push(full_path.clone());

        let md = match entry.metadata() {
            Ok(m) => m,
            Err(e) => {
                stats.errors.push(format!("{full_path}: {e}"));
                continue;
            }
        };
        use std::os::unix::fs::MetadataExt;
        let size = md.size() as i64;
        let mtime_ns = md.mtime() * 1_000_000_000 + md.mtime_nsec();

        let unchanged = known
            .get(&full_path)
            .is_some_and(|&(s, m)| s == size && m == mtime_ns);
        if unchanged {
            stats.unchanged += 1;
        }

        let ext = Path::new(&name)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        groups
            .entry((dir, c.base))
            .or_default()
            .push(Member {
                name,
                ext,
                role: c.role,
                size,
                mtime_ns,
                // Only image members carry EXIF worth parsing.
                needs_parse: !unchanged && c.role.is_image(),
            });
    }

    // --- split real stacks from orphans -------------------------------------
    // A profile or sidecar whose image is gone does not conjure a stack. This
    // library contains three such leftovers from RawTherapee.
    let (stacks, orphan_groups): (Vec<_>, Vec<_>) = groups
        .into_iter()
        .partition(|(_, members)| members.iter().any(|m| m.role.is_image()));

    stats.stacks = stacks.len();

    // --- parse metadata for new/changed image members ----------------------
    // Exiv2 is serialised behind a mutex inside `meta`, so this parallelises
    // the walk and the I/O rather than the parse. Errors are per-file: one
    // corrupt NEF must not abort the run.
    let to_parse: Vec<(String, String)> = stacks
        .iter()
        .flat_map(|((dir, _), members)| {
            members
                .iter()
                .filter(|m| m.needs_parse)
                .map(move |m| (dir.clone(), m.name.clone()))
        })
        .collect();

    let parsed: HashMap<String, meta::FileMeta> = to_parse
        .par_iter()
        .filter_map(|(dir, name)| {
            let path = format!("{dir}/{name}");
            match meta::read_meta(&path) {
                Ok(m) => Some((path, m)),
                Err(_) => None,
            }
        })
        .collect();

    let parse_failures: Vec<String> = to_parse
        .iter()
        .map(|(d, n)| format!("{d}/{n}"))
        .filter(|p| !parsed.contains_key(p))
        .collect();
    stats.parsed = parsed.len();
    for p in &parse_failures {
        stats.errors.push(format!("{p}: unreadable metadata"));
    }

    // --- write ---------------------------------------------------------------
    // (stack_id, sidecar path, its mtime) -- ingested after the transaction, so
    // tags made in darktable or digiKam are picked up on the next scan.
    let mut sidecars_found: Vec<(i64, String, i64)> = Vec::new();

    let tx = cat.conn.transaction()?;
    {
        let mut ins_stack = tx.prepare(
            "INSERT INTO stack (dir, base) VALUES (?1, ?2)
             ON CONFLICT(dir, base) DO UPDATE SET dir = excluded.dir
             RETURNING id",
        )?;
        let mut ins_file = tx.prepare(
            "INSERT INTO file (stack_id, name, ext, role, size, mtime_ns,
                               capture_time, camera, lens, iso, fnumber, exposure,
                               focal, width, height, exif_orientation, serial,
                               shutter_count, present)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,1)
             ON CONFLICT(stack_id, name) DO UPDATE SET
                 ext=excluded.ext, role=excluded.role, size=excluded.size,
                 mtime_ns=excluded.mtime_ns,
                 capture_time=COALESCE(excluded.capture_time, file.capture_time),
                 camera=COALESCE(NULLIF(excluded.camera,''), file.camera),
                 lens=COALESCE(NULLIF(excluded.lens,''), file.lens),
                 iso=COALESCE(NULLIF(excluded.iso,0), file.iso),
                 fnumber=COALESCE(NULLIF(excluded.fnumber,0.0), file.fnumber),
                 exposure=COALESCE(NULLIF(excluded.exposure,0.0), file.exposure),
                 focal=COALESCE(NULLIF(excluded.focal,0.0), file.focal),
                 width=COALESCE(NULLIF(excluded.width,0), file.width),
                 height=COALESCE(NULLIF(excluded.height,0), file.height),
                 exif_orientation=COALESCE(NULLIF(excluded.exif_orientation,0), file.exif_orientation),
                 serial=COALESCE(NULLIF(excluded.serial,''), file.serial),
                 shutter_count=COALESCE(NULLIF(excluded.shutter_count,0), file.shutter_count),
                 present=1",
        )?;

        for ((dir, base), members) in &stacks {
            let sid: i64 = ins_stack.query_row(rusqlite::params![dir, base], |r| r.get(0))?;

            // A sidecar is metadata about the stack, not a member of it. Record
            // it for ingestion below; never let it become a `file` row, or it
            // would be counted as a photograph.
            if let Some(sc) = members.iter().find(|m| m.role == Role::Sidecar) {
                sidecars_found.push((sid, format!("{dir}/{}", sc.name), sc.mtime_ns));
            }

            for m in members {
                if m.role == Role::Sidecar {
                    continue;
                }
                let path = format!("{dir}/{}", m.name);
                let fm = parsed.get(&path);
                ins_file.execute(rusqlite::params![
                    sid,
                    m.name,
                    m.ext,
                    m.role.as_str(),
                    m.size,
                    m.mtime_ns,
                    fm.map(|f| f.capture_time).filter(|&t| t != 0),
                    fm.map(|f| f.camera.as_str()).unwrap_or(""),
                    fm.map(|f| f.lens.as_str()).unwrap_or(""),
                    fm.map(|f| f.iso).unwrap_or(0),
                    fm.map(|f| f.fnumber).unwrap_or(0.0),
                    fm.map(|f| f.exposure).unwrap_or(0.0),
                    fm.map(|f| f.focal).unwrap_or(0.0),
                    fm.map(|f| f.width).unwrap_or(0),
                    fm.map(|f| f.height).unwrap_or(0),
                    fm.map(|f| f.orientation).unwrap_or(0),
                    fm.map(|f| f.serial.as_str()).unwrap_or(""),
                    fm.map(|f| f.shutter_count).unwrap_or(0),
                ])?;
            }
        }

        // Orphans: recorded, not silently dropped, so `doctor` can report them.
        tx.execute("DELETE FROM orphan", [])?;
        let now = now_secs();
        let mut ins_orphan =
            tx.prepare("INSERT OR REPLACE INTO orphan (path, kind, seen_at) VALUES (?1,?2,?3)")?;
        for ((dir, _), members) in &orphan_groups {
            for m in members {
                ins_orphan.execute(rusqlite::params![
                    format!("{dir}/{}", m.name),
                    m.role.as_str(),
                    now
                ])?;
                stats.orphans += 1;
            }
        }
    }

    // Files that disappeared are marked absent, never deleted: an unplugged
    // drive or a renamed folder must not destroy tag history.
    {
        let mut mark = tx.prepare(
            "UPDATE file SET present = 0
             WHERE present = 1
               AND (SELECT dir FROM stack WHERE stack.id = file.stack_id) LIKE ?1
               AND (SELECT dir FROM stack WHERE stack.id = file.stack_id) || '/' || name
                   NOT IN (SELECT value FROM json_each(?2))",
        )?;
        let json = serde_json_array(&seen_paths);
        let prefix = format!("{}%", root.to_string_lossy());
        stats.vanished = mark.execute(rusqlite::params![prefix, json])? ;
    }

    tx.commit()?;

    // --- ingest sidecars ------------------------------------------------------
    // Only when the sidecar's own mtime moved, so a rescan of an untouched
    // library re-reads nothing. A dirty stack is skipped: our unflushed edit is
    // newer than whatever is on disk and must not be clobbered by it.
    for (sid, path, mtime) in sidecars_found {
        let (known_mtime, dirty): (Option<i64>, i64) = cat
            .conn
            .query_row(
                "SELECT sidecar_mtime, dirty FROM stack WHERE id = ?1",
                [sid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((None, 0));

        if dirty == 1 {
            continue;
        }
        if !full && known_mtime == Some(mtime) {
            continue;
        }

        match crate::edit::ingest_sidecar(cat, sid, Path::new(&path)) {
            Ok(true) => {
                cat.conn.execute(
                    "UPDATE stack SET sidecar_mtime = ?2 WHERE id = ?1",
                    rusqlite::params![sid, mtime],
                )?;
                stats.sidecars_read += 1;
            }
            Ok(false) => {}
            Err(e) => stats.errors.push(format!("{path}: {e}")),
        }
    }

    Ok(stats)
}

/// Minimal JSON array encoder -- enough for SQLite's json_each, without adding
/// a serde_json dependency for one call site.
fn serde_json_array(items: &[String]) -> String {
    let mut s = String::with_capacity(items.len() * 48 + 2);
    s.push('[');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push('"');
        for ch in item.chars() {
            match ch {
                '"' => s.push_str("\\\""),
                '\\' => s.push_str("\\\\"),
                '\n' => s.push_str("\\n"),
                '\r' => s.push_str("\\r"),
                '\t' => s.push_str("\\t"),
                c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
                c => s.push(c),
            }
        }
        s.push('"');
    }
    s.push(']');
    s
}

/// Convenience for the CLI: identify the volume, open its catalog, scan.
pub fn scan_root(root: &Path, full: bool) -> Result<(ScanStats, Catalog)> {
    let vol = crate::vol::identify(root)?;
    let mut cat = Catalog::open_for(&vol.uuid, root)?;
    cat.register_volume(&vol.uuid, vol.label.as_deref(), root)?;
    let stats = scan(&mut cat, root, full).context("scanning")?;
    Ok((stats, cat))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_array_escapes_hostile_filenames() {
        let items = vec![
            r#"/d/quote".NEF"#.to_string(),
            "/d/back\\slash.NEF".to_string(),
            "/d/new\nline.NEF".to_string(),
        ];
        let json = serde_json_array(&items);
        assert!(json.starts_with('['));
        assert!(json.contains(r#"quote\""#));
        assert!(json.contains(r"back\\slash"));
        assert!(json.contains(r"new\nline"));
    }
}
