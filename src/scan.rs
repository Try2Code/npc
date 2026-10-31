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
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::os::unix::fs::MetadataExt;
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
    pub events: usize,
    /// Stacks that had no rating and no tags, but carried a rating and/or
    /// keywords embedded directly in an original (Shotwell, digiKam, and
    /// similar tools write these into the file itself). Seeded into the
    /// catalog and marked dirty; `npc sync` writes the sidecar.
    pub seeded_from_embedded: usize,
    /// Stacks that had no rating, no tags, and no embedded rating either,
    /// but share a basename with an already-rated stack elsewhere in the
    /// library -- the usual case being an edited export the watcher just
    /// cataloged. Seeded with that rating and marked dirty, same as
    /// `seeded_from_embedded`; kept separate only so the two sources are
    /// distinguishable in a report.
    pub inherited_rating: usize,
    pub errors: Vec<String>,
}

/// Combine embedded ratings from a stack's RAW and JPG members. If only one
/// carries a rating, use it; if both do, the higher one wins.
pub(crate) fn combine_rating(raw: Option<i32>, jpg: Option<i32>) -> Option<i32> {
    match (raw, jpg) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// When a brand-new stack carries no rating of its own (no sidecar, no
/// embedded EXIF rating), look for another stack elsewhere in the library
/// that is a version of something already rated -- the usual case being an
/// edited export the watcher just cataloged (`watch.rs`'s own doc comment:
/// "a file dropped anywhere under the watched tree becomes its own
/// independent stack"), which starts with none of the original's metadata
/// of its own. Rate the originals first, then edit, is this project's own
/// stated workflow, so this is "inherit, don't start from zero" for a file
/// that is a version of something already rated, not an attempt to keep the
/// two in sync afterward: a rating given to either one later never touches
/// the other.
///
/// Two ways to recognize "a version of", tried in order:
///
/// 1. **Camera identity** -- `serial` + `shutter_count`, the exact pair
///    `import.rs`'s own Tier A merge-target matching already trusts.
///    Confirmed against a real export rather than assumed: RawTherapee and
///    spektra both preserve the source RAW's maker-note identity in the JPG
///    they write, even when the filename itself does not survive untouched
///    (`750_9954.NEF` exported as `750_9954_spektra.jpg`). This is the
///    robust path -- it does not care what the export tool named the file.
/// 2. **Basename**, exact or as a prefix of the new stack's own basename cut
///    at a `_`/`-` boundary (`750_9954_spektra` extends `750_9954`) -- the
///    fallback for a camera with no shutter-count EXIF, or an export tool
///    that strips maker notes entirely (confirmed on a real export: a second
///    `spektra` preset ran with maker notes stripped, landing here).
///
/// Both this stack's own identity/capture_time and each candidate's are read
/// from `file`, not from this scan's freshly-parsed metadata -- the row was
/// already inserted/updated earlier in this same transaction, so this also
/// catches a stack that was cataloged before its true original existed and
/// is only now being reconsidered on a later, otherwise-unchanged rescan.
///
/// Within either path, more than one candidate is a real, confirmed failure
/// mode, not a hypothetical: this camera's file numbering recycles roughly
/// twice a year, so a basename alone can match several *unrelated* shoots
/// months apart, each already carrying its own, different, legitimate
/// rating. A same-second `capture_time` against this stack's own is checked
/// first -- proof the candidate is the same moment, not just the same
/// recycled number -- then a candidate inside the dated import tree
/// (`import`'s own `YYYY/MM/DD` convention), then whichever came back first.
///
/// `capture_time` is weaker evidence than identity and deliberately never
/// promoted above it: it comes from the camera's clock, which a user can
/// reset or let drift, where `shutter_count` is a hardware count of
/// actuations that nothing short of deliberate tampering moves. A wrong
/// guess there only mis-copies a rating -- visible and correctable, not the
/// kind of silent merge `import.rs` guards with an actual pixel check before
/// trusting this same field.
///
/// Identity, once present, is authoritative and the basename fallback is
/// never tried after it either way -- not only on a match. The real bug
/// this guards against: every fresh photo straight off a camera has a
/// `shutter_count` nothing else has ever had, so the identity query always
/// and correctly comes back empty for it. Falling through to basename on
/// that "no match" used to hand a brand-new, never-before-seen shot a stray
/// rating left over from some unrelated session that happened to reuse the
/// same low, recycled frame number (`750_0001`, the first frame of nearly
/// every card, is the worst offender) -- the opposite of "inherit, don't
/// start from zero". Basename is reached only when identity is unavailable
/// to begin with: no maker notes at all, a non-Nikon body, or an export tool
/// that stripped them.
fn find_inherited_rating(
    tx: &rusqlite::Transaction,
    root: &Path,
    base: &str,
    exclude_id: i64,
) -> Result<Option<i32>> {
    let is_dated = |dir: &str| {
        Path::new(dir)
            .strip_prefix(root)
            .ok()
            .and_then(|rel| rel.components().next())
            .is_some_and(|c| {
                let name = c.as_os_str().to_string_lossy();
                name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit())
            })
    };
    let capture_time: Option<i64> = tx
        .query_row(
            "SELECT capture_time FROM file
             WHERE stack_id = ?1 AND capture_time IS NOT NULL LIMIT 1",
            rusqlite::params![exclude_id],
            |r| r.get(0),
        )
        .optional()?;
    let pick = |candidates: Vec<(String, i32, Option<i64>)>| {
        capture_time
            .and_then(|t| candidates.iter().find(|(_, _, ct)| *ct == Some(t)))
            .or_else(|| candidates.iter().find(|(dir, _, _)| is_dated(dir)))
            .or_else(|| candidates.first())
            .map(|(_, rating, _)| *rating)
    };

    let identity: Option<(String, u32)> = tx
        .query_row(
            "SELECT serial, shutter_count FROM file
             WHERE stack_id = ?1 AND serial != '' AND shutter_count != 0 LIMIT 1",
            rusqlite::params![exclude_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((serial, shutter_count)) = identity {
        let candidates: Vec<(String, i32, Option<i64>)> = tx
            .prepare(
                "SELECT DISTINCT s.dir, s.rating,
                        (SELECT MIN(f2.capture_time) FROM file f2
                         WHERE f2.stack_id = s.id AND f2.capture_time IS NOT NULL)
                 FROM file f JOIN stack s ON s.id = f.stack_id
                 WHERE f.serial = ?1 AND f.shutter_count = ?2
                   AND s.id != ?3 AND s.rating IS NOT NULL",
            )?
            .query_map(rusqlite::params![serial, shutter_count, exclude_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        // Authoritative either way -- see the doc comment above. A
        // `shutter_count` that matches nothing is proof there is no version
        // of this shot elsewhere, not a cue to fall back to a filename guess.
        return Ok(pick(candidates));
    }

    // Basename fallback: the new stack's own base, then each of its
    // underscore/hyphen-separated prefixes, longest first, as an exact
    // match against another stack's base.
    let mut stmt = tx.prepare(
        "SELECT s.dir, s.rating,
                (SELECT MIN(f2.capture_time) FROM file f2
                 WHERE f2.stack_id = s.id AND f2.capture_time IS NOT NULL)
         FROM stack s WHERE s.base = ?1 AND s.id != ?2 AND s.rating IS NOT NULL",
    )?;
    let mut cuts: Vec<usize> = base
        .char_indices()
        .filter(|&(_, c)| c == '_' || c == '-')
        .map(|(i, _)| i)
        .collect();
    cuts.push(base.len());
    cuts.sort_unstable_by(|a, b| b.cmp(a));

    for cut in cuts {
        let candidates: Vec<(String, i32, Option<i64>)> = stmt
            .query_map(rusqlite::params![&base[..cut], exclude_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        if let Some(rating) = pick(candidates) {
            return Ok(Some(rating));
        }
    }
    Ok(None)
}

/// Union embedded tags across every member that had one, case-insensitively,
/// through the same normalisation `npc tag add` applies so casing and
/// whitespace match what a real tagging session would have produced.
pub(crate) fn union_embedded_tags<'a>(sources: impl Iterator<Item = &'a [String]>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tags in sources {
        for t in tags {
            let normalised = crate::edit::normalise_tag(t);
            if normalised.is_empty() {
                continue;
            }
            if seen.insert(normalised.to_lowercase()) {
                out.push(normalised);
            }
        }
    }
    out
}

struct Member {
    name: String,
    ext: String,
    role: Role,
    size: i64,
    mtime_ns: i64,
    needs_parse: bool,
}

/// One classified file from pass 1 of the walk, before its `stat()` (pass 2).
struct Walked {
    dir: String,
    name: String,
    full_path: String,
    base: String,
    role: Role,
}

/// A trash folder, never cataloged: npc's own `.trash/`, and the desktop's
/// on a removable drive -- `.Trash-<uid>` (freedesktop) and `.Trashes`
/// (macOS). Photos deleted in a file manager sat in `.Trash-1000` as 13
/// rated stacks of their own.
pub fn is_trash_dir(name: &str) -> bool {
    name == crate::trash::TRASH_DIR || name.starts_with(".Trash-") || name == ".Trashes"
}

/// Scan `root` into the catalog. Originals are only ever opened read-only, so
/// this cannot alter the drive even if it panics mid-walk.
pub fn scan(cat: &mut Catalog, root: &Path, full: bool) -> Result<ScanStats> {
    // Stack dirs are stored as walked. `npc scan --only .` once stored 1,834
    // stacks as `./4cdsh` and the like: duplicates of their absolute twins,
    // whose sidecars resolve against whatever the browser's cwd happens to
    // be. Absolute, but not canonical -- a symlinked library keeps the path
    // it was configured with, which is what every other path is built from.
    let root =
        &std::path::absolute(root).with_context(|| format!("resolving {}", root.display()))?;
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

    // Pass 1: walk and classify only -- no stat() yet. `file_type()` costs
    // nothing extra on filesystems that supply `d_type` in the dirent
    // (ext4/exFAT/vfat all do); only `entry.metadata()` below forces a real
    // syscall, which is what gets parallelised in pass 2.
    let mut walked: Vec<Walked> = Vec::new();
    for entry in WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_trash_dir(&e.file_name().to_string_lossy()))
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

        walked.push(Walked {
            dir,
            name,
            full_path,
            base: c.base,
            role: c.role,
        });
    }

    // Pass 2: stat every file in parallel. Not behind the Exiv2 mutex, so
    // this is a genuine win on a large tree, especially over a slow
    // removable drive. `symlink_metadata` matches what `DirEntry::metadata()`
    // does under `follow_links(false)`. `par_iter().map().collect::<Vec<_>>()`
    // on an indexed iterator preserves order, so pass 3 can zip results back
    // to `walked` positionally with no extra bookkeeping.
    let stats_results: Vec<Result<(i64, i64), String>> = walked
        .par_iter()
        .map(|w| {
            std::fs::symlink_metadata(&w.full_path)
                .map(|m| {
                    let size = i64::try_from(m.size()).unwrap_or(i64::MAX);
                    let mtime_ns = m.mtime() * 1_000_000_000 + m.mtime_nsec();
                    (size, mtime_ns)
                })
                .map_err(|e| format!("{}: {e}", w.full_path))
        })
        .collect();

    // Pass 3: sequential, same shape as the old single-pass loop.
    for (w, result) in walked.into_iter().zip(stats_results) {
        let (size, mtime_ns) = match result {
            Ok(v) => v,
            Err(e) => {
                stats.errors.push(e);
                continue;
            }
        };

        let unchanged = known
            .get(&w.full_path)
            .is_some_and(|&(s, m)| s == size && m == mtime_ns);
        if unchanged {
            stats.unchanged += 1;
        }

        let ext = Path::new(&w.name)
            .extension()
            .map(|e| e.to_string_lossy().to_string())
            .unwrap_or_default();

        groups.entry((w.dir, w.base)).or_default().push(Member {
            name: w.name,
            ext,
            role: w.role,
            size,
            mtime_ns,
            // Only image members carry EXIF worth parsing.
            needs_parse: !unchanged && w.role.is_image(),
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
                               shutter_count, exposure_bias, present)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,1)
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
                 exposure_bias=COALESCE(excluded.exposure_bias, file.exposure_bias),
                 present=1",
        )?;

        // Seeding embedded rating/tags: only for a stack that has neither yet,
        // so a real edit (via npc, or a sidecar ingested below) is never
        // clobbered. This marks the stack dirty in the catalog only -- it is
        // not a write to the library. `npc sync` is what writes the sidecar,
        // through the same `write_sidecar` this project already audits.
        let mut stack_state =
            tx.prepare("SELECT rating, (SELECT COUNT(*) FROM stack_tag WHERE stack_id = s.id) FROM stack s WHERE id = ?1")?;
        let mut ins_tag = tx.prepare("INSERT OR IGNORE INTO tag (name) VALUES (?1)")?;
        let mut sel_tag_id = tx.prepare("SELECT id FROM tag WHERE name = ?1")?;
        let mut ins_stack_tag =
            tx.prepare("INSERT OR IGNORE INTO stack_tag (stack_id, tag_id) VALUES (?1,?2)")?;
        let mut seed_stack = tx.prepare("UPDATE stack SET rating = ?2, dirty = 1 WHERE id = ?1")?;
        let mut ins_pending =
            tx.prepare("INSERT OR REPLACE INTO pending (stack_id, queued_at) VALUES (?1, ?2)")?;
        for ((dir, base), members) in &stacks {
            let sid: i64 = ins_stack.query_row(rusqlite::params![dir, base], |r| r.get(0))?;

            // A sidecar is metadata about the stack, not a member of it. Record
            // it for ingestion below; never let it become a `file` row, or it
            // would be counted as a photograph.
            let has_sidecar = members.iter().any(|m| m.role == Role::Sidecar);
            if let Some(sc) = members.iter().find(|m| m.role == Role::Sidecar) {
                sidecars_found.push((sid, format!("{dir}/{}", sc.name), sc.mtime_ns));
            }

            let mut raw_rating = None;
            let mut jpg_rating = None;
            let mut embedded_tags: Vec<&[String]> = Vec::new();

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
                    fm.map_or("", |f| f.camera.as_str()),
                    fm.map_or("", |f| f.lens.as_str()),
                    fm.map_or(0, |f| f.iso),
                    fm.map_or(0.0, |f| f.fnumber),
                    fm.map_or(0.0, |f| f.exposure),
                    fm.map_or(0.0, |f| f.focal),
                    fm.map_or(0, |f| f.width),
                    fm.map_or(0, |f| f.height),
                    fm.map_or(0, |f| f.orientation),
                    fm.map_or("", |f| f.serial.as_str()),
                    fm.map_or(0, |f| f.shutter_count),
                    fm.and_then(|f| f.has_bias.then_some(f.exposure_bias)),
                ])?;

                if let Some(fm) = fm {
                    let rating = (fm.rating >= 0).then_some(fm.rating);
                    match m.role {
                        Role::Raw => raw_rating = rating,
                        Role::OocJpg => jpg_rating = rating,
                        _ => {}
                    }
                    if !fm.tags.is_empty() {
                        embedded_tags.push(&fm.tags);
                    }
                }
            }

            // A stack with a real sidecar on disk must never be seeded from
            // embedded values, dirty or not: marking it dirty here would make
            // the sidecar-ingestion pass below skip it (a dirty stack's
            // unflushed edit is treated as newer than the file, by design),
            // which would silently block a real, possibly-since-cleared
            // sidecar from ever being read back. The sidecar always wins, and
            // the only way to guarantee that is to not touch this stack here.
            let (existing_rating, existing_tags): (Option<i64>, i64) =
                stack_state.query_row(rusqlite::params![sid], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if !has_sidecar && existing_rating.is_none() && existing_tags == 0 {
                let mut rating = combine_rating(raw_rating, jpg_rating);
                let inherited = rating.is_none()
                    && match find_inherited_rating(&tx, root, base, sid)? {
                        found @ Some(_) => {
                            rating = found;
                            true
                        }
                        None => false,
                    };
                let tags = union_embedded_tags(embedded_tags.into_iter());
                if rating.is_some() || !tags.is_empty() {
                    seed_stack.execute(rusqlite::params![sid, rating.map(i64::from)])?;
                    for t in &tags {
                        ins_tag.execute(rusqlite::params![t])?;
                        let tid: i64 = sel_tag_id.query_row(rusqlite::params![t], |r| r.get(0))?;
                        ins_stack_tag.execute(rusqlite::params![sid, tid])?;
                    }
                    ins_pending.execute(rusqlite::params![sid, now_secs()])?;
                    if inherited {
                        stats.inherited_rating += 1;
                    } else {
                        stats.seeded_from_embedded += 1;
                    }
                }
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
        stats.vanished = mark.execute(rusqlite::params![prefix, json])?;
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

    stats.events = derive_events(cat)?;

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
                c if (c as u32) < 0x20 => {
                    let _ = write!(s, "\\u{:04x}", c as u32);
                }
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

/// Group stacks into events: one per day of shooting.
///
/// The day comes from capture time, never from the file's mtime or its
/// directory. That is what makes a JPG exported from a NEF months later land
/// in the same event as the RAW it came from -- every sane converter copies
/// DateTimeOriginal across, so the derived file dates itself to the shoot
/// rather than to the export.
///
/// User-given names survive: events are keyed on the day, so re-deriving finds
/// the same row and leaves `name` alone.
pub fn derive_events(cat: &mut Catalog) -> Result<usize> {
    let tx = cat.conn.transaction()?;

    // A day, as the same as-if-UTC convention capture times are stored in.
    tx.execute(
        "INSERT OR IGNORE INTO event (start_time, end_time, auto)
         SELECT DISTINCT
             CAST(strftime('%s', date(f.capture_time, 'unixepoch')) AS INTEGER),
             CAST(strftime('%s', date(f.capture_time, 'unixepoch')) AS INTEGER) + 86399,
             1
         FROM file f
         WHERE f.capture_time IS NOT NULL",
        [],
    )?;

    tx.execute(
        "UPDATE stack SET event_id = (
             SELECT e.id FROM event e
             WHERE e.start_time = CAST(strftime('%s', date(
                 (SELECT MIN(f.capture_time) FROM file f
                   WHERE f.stack_id = stack.id AND f.capture_time IS NOT NULL),
                 'unixepoch')) AS INTEGER))",
        [],
    )?;

    // An event nothing points at is a leftover from deleted shots. Named ones
    // are kept: the user said something about that day and may restore it.
    tx.execute(
        "DELETE FROM event
         WHERE name IS NULL
           AND id NOT IN (SELECT event_id FROM stack WHERE event_id IS NOT NULL)",
        [],
    )?;

    let n: i64 = tx.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?;
    tx.commit()?;
    Ok(usize::try_from(n).unwrap_or(0))
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

    #[test]
    fn rating_combines_by_your_rule() {
        // Only one member rated: use it.
        assert_eq!(combine_rating(Some(3), None), Some(3));
        assert_eq!(combine_rating(None, Some(4)), Some(4));
        // Both rated: the higher wins.
        assert_eq!(combine_rating(Some(2), Some(5)), Some(5));
        assert_eq!(combine_rating(Some(5), Some(2)), Some(5));
        assert_eq!(combine_rating(Some(3), Some(3)), Some(3));
        // Neither rated: nothing to seed.
        assert_eq!(combine_rating(None, None), None);
    }

    #[test]
    fn tags_union_case_insensitively_and_normalise() {
        let nef = vec!["India".to_string(), "  berlin  ".to_string()];
        let jpg = vec!["india".to_string(), "Carsten Geburtstag".to_string()];
        let tags = union_embedded_tags([nef.as_slice(), jpg.as_slice()].into_iter());
        // "India"/"india" collapse to one, casing from whichever was seen first.
        assert_eq!(tags, vec!["India", "berlin", "Carsten Geburtstag"]);
    }

    #[test]
    fn empty_and_blank_tags_are_dropped() {
        let a = vec![String::new(), "   ".to_string()];
        let tags = union_embedded_tags([a.as_slice()].into_iter());
        assert!(tags.is_empty());
    }

    #[test]
    fn no_embedded_tags_anywhere_is_an_empty_union() {
        let tags = union_embedded_tags(std::iter::empty());
        assert!(tags.is_empty());
    }

    use crate::config::Config;
    use crate::edit::{self, Change};

    /// The exact workflow this feature exists for: rate the originals
    /// first, edit later. A RawTherapee/spektra export keeps the source's
    /// filename stem but starts with none of its metadata -- it should
    /// Nothing in a trash folder is cataloged -- npc's own, or the desktop's
    /// on the drive.
    #[test]
    fn trash_folders_are_never_cataloged() {
        let dir = tempfile::tempdir().unwrap();
        for sub in [
            "2026/08/09",
            ".trash/2026/08/09",
            ".Trash-1000/files",
            ".Trashes/501",
        ] {
            let d = dir.path().join(sub);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("750_0001.JPG"), "jpg").unwrap();
        }
        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();
        let dirs: Vec<String> = cat
            .conn
            .prepare("SELECT dir FROM stack")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            dirs,
            vec![dir.path().join("2026/08/09").to_string_lossy().to_string()]
        );
    }

    /// A relative root is cataloged under its absolute path. Stored as given,
    /// `./4cdsh` stacks duplicated their absolute twins and their sidecars
    /// resolved against the browser's cwd -- "can't be written" in npcg.
    #[test]
    fn a_relative_root_is_stored_as_absolute_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("export/4cdsh");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("750_5610.JPG"), "jpg").unwrap();

        // Relative to the cwd without changing it (process-global in tests):
        // `..` up to `/`, then down to the target.
        let cwd = std::env::current_dir().unwrap();
        let mut relative = std::path::PathBuf::new();
        for _ in cwd.components().skip(1) {
            relative.push("..");
        }
        relative.push(dir.path().join("export").strip_prefix("/").unwrap());
        assert!(relative.is_relative());

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, &relative, true).unwrap();

        let dirs: Vec<String> = cat
            .conn
            .prepare("SELECT dir FROM stack")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(dirs.len(), 1);
        assert!(Path::new(&dirs[0]).is_absolute(), "stored {:?}", dirs[0]);
        assert!(Path::new(&dirs[0]).join("750_5610.JPG").is_file());
    }

    /// inherit the rating already given to the original, not start unrated.
    #[test]
    fn a_new_stack_inherits_the_rating_of_a_same_named_stack_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let original_dir = dir.path().join("2026/01/01");
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("750_0001.NEF"), "raw").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let original_id: i64 = cat
            .conn
            .query_row("SELECT id FROM stack WHERE base = '750_0001'", [], |r| {
                r.get(0)
            })
            .unwrap();
        edit::apply(
            &mut cat,
            &[original_id],
            &[Change::SetRating(Some(4))],
            &Config::default(),
        )
        .unwrap();

        // The edited export appears later, elsewhere in the library -- no
        // sidecar, no embedded rating of its own (fixture bytes carry none).
        let export_dir = dir.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("750_0001.JPG"), "jpg").unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(stats.inherited_rating, 1);

        let inherited: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE dir = ?1 AND base = '750_0001'",
                [export_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inherited, Some(4));

        // Inheritance is one-way and one-time: the original's own rating is
        // never touched by cataloging the export.
        let original_rating: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE id = ?1",
                [original_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(original_rating, Some(4));
    }

    /// "I might give them a different rating later" -- a rating the user set
    /// by hand, however it got there, must never be clobbered by a later
    /// rescan just because a same-named stack elsewhere also has one.
    #[test]
    fn a_stacks_own_rating_is_never_overwritten_by_inheritance() {
        let dir = tempfile::tempdir().unwrap();
        let original_dir = dir.path().join("2026/01/01");
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("750_0001.NEF"), "raw").unwrap();
        let export_dir = dir.path().join("export");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("750_0001.JPG"), "jpg").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let original_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [original_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let export_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [export_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();

        edit::apply(
            &mut cat,
            &[export_id],
            &[Change::SetRating(Some(2))],
            &Config::default(),
        )
        .unwrap();
        edit::apply(
            &mut cat,
            &[original_id],
            &[Change::SetRating(Some(5))],
            &Config::default(),
        )
        .unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(
            stats.inherited_rating, 0,
            "the export already had its own rating"
        );

        let export_rating: Option<i64> = cat
            .conn
            .query_row("SELECT rating FROM stack WHERE id = ?1", [export_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(export_rating, Some(2));
    }

    /// Two same-named stacks could exist outside the dated tree too (two
    /// different exports of the same shot). The one actually inside it is
    /// unambiguously "the original" and must win the tie-break.
    #[test]
    fn inheritance_prefers_a_candidate_inside_the_dated_import_tree() {
        let dir = tempfile::tempdir().unwrap();
        let original_dir = dir.path().join("2026/01/01");
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("750_0001.NEF"), "raw").unwrap();
        let other_export_dir = dir.path().join("export/a");
        std::fs::create_dir_all(&other_export_dir).unwrap();
        std::fs::write(other_export_dir.join("750_0001.JPG"), "jpg-a").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let original_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [original_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let other_export_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [other_export_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        edit::apply(
            &mut cat,
            &[original_id],
            &[Change::SetRating(Some(5))],
            &Config::default(),
        )
        .unwrap();
        edit::apply(
            &mut cat,
            &[other_export_id],
            &[Change::SetRating(Some(1))],
            &Config::default(),
        )
        .unwrap();

        let new_export_dir = dir.path().join("export/b");
        std::fs::create_dir_all(&new_export_dir).unwrap();
        std::fs::write(new_export_dir.join("750_0001.JPG"), "jpg-b").unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let new_rating: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE dir = ?1",
                [new_export_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            new_rating,
            Some(5),
            "should inherit from the dated-tree original, not the other export"
        );
    }

    /// The real bug: `spektra` renames `750_9954.NEF` to `750_9954_spektra.jpg`
    /// on export, so the basename match alone finds nothing. Fixture bytes
    /// carry no real EXIF, so this stamps `file.serial`/`shutter_count`
    /// directly after scanning -- the same synthetic-EXIF-value pattern used
    /// elsewhere in this suite -- to prove identity is tried even when the
    /// filename has a suffix appended.
    #[test]
    fn a_renamed_export_inherits_the_rating_via_camera_identity_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        let original_dir = dir.path().join("2026/01/01");
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("750_9954.NEF"), "raw").unwrap();
        let export_dir = dir.path().join("export/color");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("750_9954_spektra.jpg"), "jpg").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let original_id: i64 = cat
            .conn
            .query_row("SELECT id FROM stack WHERE base = '750_9954'", [], |r| {
                r.get(0)
            })
            .unwrap();
        cat.conn
            .execute(
                "UPDATE file SET serial = '6094708', shutter_count = 67177
                 WHERE stack_id IN (
                     SELECT id FROM stack WHERE base IN ('750_9954', '750_9954_spektra')
                 )",
                [],
            )
            .unwrap();
        edit::apply(
            &mut cat,
            &[original_id],
            &[Change::SetRating(Some(2))],
            &Config::default(),
        )
        .unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(stats.inherited_rating, 1);

        let inherited: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE base = '750_9954_spektra'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inherited, Some(2));
    }

    /// Reproduces the real bug behind a lot of freshly imported photos
    /// turning up already rated: `750_0001` is the first frame of nearly
    /// every card from a long-lived body, so the library already holds
    /// several old, legitimately-rated stacks under that exact basename.
    /// A brand-new shot sharing the number but carrying its own, never
    /// -before-seen `shutter_count` must come back unrated -- identity
    /// finding no match is proof nothing else is a version of this shot, not
    /// a cue to fall back to the basename and grab a stranger's rating.
    #[test]
    fn identity_present_but_unmatched_never_falls_back_to_a_recycled_basename() {
        let dir = tempfile::tempdir().unwrap();
        let old_dir = dir.path().join("2025/04/26");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::write(old_dir.join("750_0001.NEF"), "raw-old").unwrap();
        let new_dir = dir.path().join("2026/10/03");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("750_0001.NEF"), "raw-new").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let old_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [old_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let new_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [new_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        // Same body, two different lifetime shutter actuations -- exactly
        // what distinguishes a real recycled-basename collision from an
        // actual re-export of the same shot.
        cat.conn
            .execute(
                "UPDATE file SET serial = '6094708', shutter_count = 59662 WHERE stack_id = ?1",
                [old_id],
            )
            .unwrap();
        cat.conn
            .execute(
                "UPDATE file SET serial = '6094708', shutter_count = 67223 WHERE stack_id = ?1",
                [new_id],
            )
            .unwrap();
        edit::apply(
            &mut cat,
            &[old_id],
            &[Change::SetRating(Some(1))],
            &Config::default(),
        )
        .unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(stats.inherited_rating, 0);

        let new_rating: Option<i64> = cat
            .conn
            .query_row("SELECT rating FROM stack WHERE id = ?1", [new_id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            new_rating, None,
            "a never-before-seen shutter_count must not inherit a stranger's rating \
             just because the recycled filename matches"
        );
    }

    /// Camera identity is unavailable (no maker notes in these fixture bytes,
    /// same as a non-Nikon body or an export tool that strips them) but the
    /// renamed export's basename still extends the original's at a `_`
    /// boundary -- the fallback path must still find it.
    #[test]
    fn a_renamed_export_inherits_the_rating_via_basename_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let original_dir = dir.path().join("2026/01/01");
        std::fs::create_dir_all(&original_dir).unwrap();
        std::fs::write(original_dir.join("750_9954.NEF"), "raw").unwrap();
        let export_dir = dir.path().join("export/color");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("750_9954_spektra.jpg"), "jpg").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let original_id: i64 = cat
            .conn
            .query_row("SELECT id FROM stack WHERE base = '750_9954'", [], |r| {
                r.get(0)
            })
            .unwrap();
        edit::apply(
            &mut cat,
            &[original_id],
            &[Change::SetRating(Some(3))],
            &Config::default(),
        )
        .unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(stats.inherited_rating, 1);

        let inherited: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE base = '750_9954_spektra'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(inherited, Some(3));
    }

    /// The real bug behind `750_9943`: this camera's own file-numbering
    /// recycles, so the same basename shows up on unrelated shoots months
    /// apart, each already carrying its own real rating. A basename-only
    /// match would grab whichever candidate the query happened to return
    /// first; this proves the export's own `capture_time` -- which an
    /// export tool preserves even when maker notes are stripped -- picks
    /// the genuinely matching shoot instead.
    #[test]
    fn basename_fallback_prefers_the_candidate_with_a_matching_capture_time() {
        let dir = tempfile::tempdir().unwrap();
        let old_shoot_dir = dir.path().join("2025/04/26");
        std::fs::create_dir_all(&old_shoot_dir).unwrap();
        std::fs::write(old_shoot_dir.join("750_9943.NEF"), "raw-old").unwrap();
        let new_shoot_dir = dir.path().join("2026/10/03");
        std::fs::create_dir_all(&new_shoot_dir).unwrap();
        std::fs::write(new_shoot_dir.join("750_9943.NEF"), "raw-new").unwrap();
        let export_dir = dir.path().join("export/color");
        std::fs::create_dir_all(&export_dir).unwrap();
        std::fs::write(export_dir.join("750_9943.jpg"), "jpg").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        scan(&mut cat, dir.path(), true).unwrap();

        let old_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [old_shoot_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let new_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM stack WHERE dir = ?1",
                [new_shoot_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        edit::apply(
            &mut cat,
            &[old_id],
            &[Change::SetRating(Some(1))],
            &Config::default(),
        )
        .unwrap();
        edit::apply(
            &mut cat,
            &[new_id],
            &[Change::SetRating(Some(2))],
            &Config::default(),
        )
        .unwrap();

        cat.conn
            .execute(
                "UPDATE file SET capture_time = 1745689767 WHERE stack_id = ?1",
                [old_id],
            )
            .unwrap();
        cat.conn
            .execute(
                "UPDATE file SET capture_time = 1791019975 WHERE stack_id = ?1",
                [new_id],
            )
            .unwrap();
        cat.conn
            .execute(
                "UPDATE file SET capture_time = 1791019975
                 WHERE stack_id IN (SELECT id FROM stack WHERE dir = ?1)",
                [export_dir.to_str().unwrap()],
            )
            .unwrap();

        let stats = scan(&mut cat, dir.path(), true).unwrap();
        assert_eq!(stats.inherited_rating, 1);

        let inherited: Option<i64> = cat
            .conn
            .query_row(
                "SELECT rating FROM stack WHERE dir = ?1",
                [export_dir.to_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            inherited,
            Some(2),
            "should match the same-moment shoot, not an unrelated one sharing the recycled number"
        );
    }
}
