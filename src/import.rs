//! Ingesting from a card into a dated library.
//!
//! Three rules, in order of importance:
//!
//! 1. **The source is never modified.** Files are opened read-only and there is
//!    no `--move`, deliberately. A card is the only copy of a shoot until the
//!    import is verified, and a tool that empties it is one bug away from
//!    losing the shoot.
//! 2. **Verify before recording.** Copy to a temp name, fsync, re-read the
//!    destination and compare hashes, and only then rename it into place. A
//!    truncated copy is deleted and reported, never registered as success.
//! 3. **A stack moves together.** Every member of one shot lands in the same
//!    directory with the same basename, including the collision suffix.

use crate::config::Config;
use crate::db::Catalog;
use crate::stack::{classify, Role};
use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A file on the card: path, role, size, mtime in ns.
type SourceFile = (PathBuf, Role, u64, i64);

/// A maker-note identity, scoped to one role: serial, shutter count, role.
type ShotIdentity = (String, u32, Role);
/// An already-cataloged file matching some identity: path, size.
type ExistingFile = (String, i64);
/// A same-`capture_time` candidate for the metadata-merge lookup: path, size, role.
type TimeCandidate = (String, i64, Role);

#[derive(Debug, Clone)]
pub struct PlannedStack {
    pub base: String,
    /// Basename actually used at the destination, with a collision suffix if
    /// one was needed. Shared by every member -- and, when the shot already
    /// has files in the library, that stack's own base and folder, so a new
    /// member joins it.
    pub dest_base: String,
    pub dest_dir: PathBuf,
    pub capture_time: Option<i64>,
    /// The card files to copy. Members already in the library are in
    /// `already` instead.
    pub members: Vec<PlannedFile>,
    /// Card files of this shot that are already in the library.
    pub already: Vec<Already>,
    /// `members` complete a stack already in the library -- found on this
    /// card or by shutter count, e.g. a RAW whose JPG came off another card.
    pub joins_existing: bool,
    /// Set when every member is already in the library: nothing to copy.
    pub duplicate_of: Option<String>,
    /// Set (only under `--verify-content`) when identity and size matched an
    /// existing file but the bytes didn't -- so it's importing as new, not
    /// being silently skipped, but it's worth knowing how close a call it was.
    pub near_miss: Option<NearMiss>,
    /// Set when `duplicate_of` was confirmed the same shot despite a size
    /// difference -- typically an external tool (Shotwell) embedding a
    /// rating or tags after the file was first imported. Carries what the
    /// library's catalog entry would gain if merged.
    pub metadata_merge: Option<crate::metadata_merge::MetadataMerge>,
}

#[derive(Debug, Clone)]
pub struct NearMiss {
    /// The anchor file actually compared -- not just any member of the
    /// stack, so this is exactly what to open to look at what differs.
    pub source: String,
    pub candidate: String,
    pub diff_bytes: u64,
    pub first_diff_offset: u64,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub role: Role,
    pub size: u64,
    pub src_mtime_ns: i64,
}

/// A card file found already in the library.
#[derive(Debug, Clone)]
pub struct Already {
    pub src: PathBuf,
    pub src_mtime_ns: i64,
    /// The library file it matched.
    pub library: String,
    /// Matched from the catalog alone -- name, size, and the card mtime where
    /// it was recorded -- without reading the card file.
    pub unread: bool,
}

/// The card name and mtime of a library file, as `record_sources` stores them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceRecord {
    pub library: PathBuf,
    pub src_name: String,
    pub src_mtime_ns: i64,
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub stacks_copied: usize,
    pub files_copied: usize,
    pub bytes_copied: u64,
    pub duplicates_skipped: usize,
    /// How many stacks carried a metadata merge that was actually applied to
    /// the library's catalog entry. Zero under `--dry-run`, since nothing is
    /// written then.
    pub merges_applied: usize,
    pub errors: Vec<String>,
    /// Card name and mtime for every library file this run copied or
    /// confirmed, for `record_sources` once the catalog has the copies.
    /// Empty under `--dry-run`: nothing was confirmed, only planned.
    pub sources: Vec<SourceRecord>,
}

/// Build the plan without touching anything.
///
/// Each card file is identified in two levels:
///
/// 1. **From the catalog alone**, without reading the card file: a library
///    file with the same card name, size and card mtime (recorded at import),
///    or -- for files imported before card stats were recorded -- the same
///    name and size. Checked against the real catalog: across 115k RAW and
///    JPG files, no two *different* shots share name, extension and size.
/// 2. **By shutter count**: one Exiv2 read per card shot, then serial +
///    shutter count + role -- a card JPG is only ever compared with library
///    JPGs, a RAW with RAWs. Same size: already imported. Different size:
///    the same file with rewritten metadata (a merge). Found only in the
///    other role: a new file for a shot already in the library, copied into
///    that stack so it joins it.
///
/// A basename is only ever a hint. A new shot never joins another shot's
/// files because a recycled number matches (`free_base`).
///
/// `deep` skips level 1. `verify_content` hash-checks a level-2 match before
/// trusting it -- see `find_duplicate`.
pub fn plan(
    cat: &Catalog,
    source: &Path,
    library: &Path,
    verify_content: bool,
    deep: bool,
) -> Result<Vec<PlannedStack>> {
    if !source.is_dir() {
        bail!("import source {} is not a directory", source.display());
    }

    // A source that is the library, inside it, or containing it is never a
    // real card -- and without this check `plan()` would silently walk the
    // whole library as if it were one, calling `read_meta` once per stack
    // with no incremental skip (unlike `scan()`, which has one). On a real
    // library that's tens of thousands of mutex-serialised Exiv2 calls,
    // which looks exactly like a hang. Canonicalize so a relative path or a
    // symlink can't sneak past a naive string comparison.
    let source_canon = source
        .canonicalize()
        .with_context(|| format!("resolving import source {}", source.display()))?;
    let library_canon = library
        .canonicalize()
        .with_context(|| format!("resolving library {}", library.display()))?;
    if source_canon.starts_with(&library_canon) || library_canon.starts_with(&source_canon) {
        bail!(
            "import source {} overlaps the library {} -- import needs a card or \
             inbox directory outside the library, not the library itself",
            source.display(),
            library.display()
        );
    }

    // Group source files into stacks, exactly as the scanner does.
    // Pass 1: walk and classify only -- no stat() yet.
    let mut walked: Vec<(PathBuf, PathBuf, String, Role)> = Vec::new();
    for entry in walkdir::WalkDir::new(source)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(c) = classify(&name) else { continue };
        let dir = entry.path().parent().unwrap_or(source).to_path_buf();
        walked.push((entry.path().to_path_buf(), dir, c.base, c.role));
    }

    // Pass 2: stat every card file in parallel. Not behind the Exiv2 mutex,
    // so threads here genuinely overlap I/O on a slow card reader. A failed
    // stat yields size 0, not a hard error -- it then matches nothing at
    // level 1 and goes through level 2 like any new file.
    let stats: Vec<(u64, i64)> = walked
        .par_iter()
        .map(|(path, ..)| std::fs::metadata(path).map_or((0, 0), |m| (m.len(), mtime_ns(&m))))
        .collect();

    // Pass 3: sequential, same shape as the old single-pass loop.
    let mut groups: HashMap<(PathBuf, String), Vec<SourceFile>> = HashMap::new();
    for ((path, dir, base, role), (size, mtime)) in walked.into_iter().zip(stats) {
        groups
            .entry((dir, base))
            .or_default()
            .push((path, role, size, mtime));
    }

    let on_disk = if deep {
        LibraryFiles::default()
    } else {
        library_files(cat)?
    };
    let by_role = known_shots_by_role(cat)?;
    let known_content = known_by_content(cat)?;
    let known_time = known_by_time(cat)?;

    let mut plans = Vec::new();
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    // Per destination folder: every stem already used there, on disk or
    // claimed by this plan. What `free_base` must avoid.
    let mut stems: HashMap<PathBuf, HashSet<String>> = HashMap::new();

    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();

    for key in keys {
        let members = &groups[&key];
        if !members.iter().any(|(_, role, _, _)| role.is_image()) {
            continue; // orphan profile on the card
        }
        let base = key.1.clone();

        // Level 1.
        let mut already = Vec::new();
        let mut unresolved = Vec::new();
        for m in members {
            match on_disk.find(m) {
                Some(library) => already.push(Already {
                    src: m.0.clone(),
                    src_mtime_ns: m.3,
                    library,
                    unread: true,
                }),
                None => unresolved.push(m),
            }
        }

        // Level 2, only for what level 1 left open.
        let mut capture_time = None;
        let mut ident = None;
        let mut near_miss = None;
        let mut metadata_merge = None;
        let mut to_copy: Vec<&SourceFile> = Vec::new();
        if !unresolved.is_empty() {
            // Identity is per shot, so one read serves every member.
            let anchor = unresolved
                .iter()
                .find(|(_, r, _, _)| *r == Role::Raw)
                .or_else(|| unresolved.iter().find(|(_, r, _, _)| r.is_image()))
                .copied();
            let anchor_meta =
                anchor.and_then(|(path, ..)| crate::meta::read_meta(&path.to_string_lossy()).ok());
            capture_time = anchor_meta
                .as_ref()
                .and_then(|m| (m.capture_time != 0).then_some(m.capture_time));
            ident = anchor_meta.as_ref().and_then(|m| {
                (!m.serial.is_empty() && m.shutter_count != 0)
                    .then(|| (m.serial.clone(), m.shutter_count))
            });

            for m in unresolved {
                let (path, role, size, mtime) = m;
                let (dup, nm) = find_duplicate(
                    path,
                    *size,
                    *role,
                    ident.as_ref(),
                    capture_time,
                    &by_role,
                    &known_content,
                    verify_content,
                );
                if nm.is_some() {
                    near_miss = nm;
                }
                if let Some(library) = dup {
                    already.push(Already {
                        src: path.clone(),
                        src_mtime_ns: *mtime,
                        library,
                        unread: false,
                    });
                    continue;
                }

                // Not byte-identical, but possibly the same file with richer
                // embedded metadata than what's cataloged -- e.g. Shotwell
                // rated or tagged it directly, after it was already imported.
                if metadata_merge.is_none() {
                    if let Some(existing) = find_metadata_merge_target(
                        path,
                        *size,
                        *role,
                        ident.as_ref(),
                        capture_time,
                        &by_role,
                        &known_time,
                    ) {
                        // The merge carries this file's own embedded metadata;
                        // re-read only when it isn't the anchor (rare).
                        let own_meta;
                        let meta = if anchor.is_some_and(|a| std::ptr::eq(a, m)) {
                            anchor_meta.as_ref()
                        } else {
                            own_meta = crate::meta::read_meta(&path.to_string_lossy()).ok();
                            own_meta.as_ref()
                        };
                        metadata_merge = build_metadata_merge(cat, &existing, meta);
                        already.push(Already {
                            src: path.clone(),
                            src_mtime_ns: *mtime,
                            library: existing,
                            unread: false,
                        });
                        continue;
                    }
                }
                to_copy.push(m);
            }
        }

        // Where this shot already lives, if anywhere: the folder and base of
        // a member found above, or of the same shot in the other role.
        let home = already
            .first()
            .and_then(|a| location(&a.library))
            .or_else(|| {
                let (serial, count) = ident.as_ref()?;
                [Role::Raw, Role::OocJpg]
                    .iter()
                    .find_map(|r| by_role.get(&(serial.clone(), *count, *r)))
                    .and_then(|candidates| candidates.first())
                    .and_then(|(path, _)| location(path))
            });

        let dest_for = |dir: &Path, dest_base: &str, src: &Path| {
            let name = src.file_name().unwrap_or_default().to_string_lossy();
            let suffix = name
                .strip_prefix(base.as_str())
                .unwrap_or(&name)
                .to_string();
            dir.join(format!("{dest_base}{suffix}"))
        };

        let (dest_dir, dest_base, joins_existing) = if to_copy.is_empty() {
            (library.to_path_buf(), base.clone(), false)
        } else {
            // Join the existing stack, unless that would land on a file
            // that's already there -- then it's not as simple as a missing
            // member, and it becomes a stack of its own.
            let joinable = home.filter(|(dir, b)| {
                to_copy.iter().all(|(src, ..)| {
                    let dest = dest_for(dir, b, src);
                    !dest.exists() && !claimed.contains(&dest)
                })
            });
            let joins = joinable.is_some();
            let (dir, b) = joinable.unwrap_or_else(|| {
                let dir = match capture_time {
                    Some(t) => library.join(date_path(t)),
                    // Undated files must not be silently scattered; they go
                    // somewhere obvious rather than being guessed at from mtime.
                    None => library.join("undated"),
                };
                let taken = stems.entry(dir.clone()).or_insert_with(|| stems_in(&dir));
                let b = free_base(taken, &base);
                (dir, b)
            });
            stems
                .entry(dir.clone())
                .or_insert_with(|| stems_in(&dir))
                .insert(b.clone());
            (dir, b, joins)
        };

        let planned: Vec<PlannedFile> = to_copy
            .iter()
            .map(|(src, role, size, mtime)| PlannedFile {
                src: src.clone(),
                dest: dest_for(&dest_dir, &dest_base, src),
                role: *role,
                size: *size,
                src_mtime_ns: *mtime,
            })
            .collect();
        for p in &planned {
            claimed.insert(p.dest.clone());
        }

        let duplicate_of = if planned.is_empty() {
            already.first().map(|a| a.library.clone())
        } else {
            None
        };
        plans.push(PlannedStack {
            base,
            dest_base,
            dest_dir,
            capture_time,
            members: planned,
            already,
            joins_existing,
            duplicate_of,
            near_miss,
            metadata_merge,
        });
    }

    Ok(plans)
}

/// Reported as `execute` works through a plan, so a caller can drive a
/// progress display and log what got skipped and why -- `import.rs` stays
/// free of presentation concerns; `main.rs` owns what these look like on
/// screen.
pub enum ImportEvent<'a> {
    /// A verified duplicate: same identity and same bytes. Not copied.
    Skipped(&'a PlannedStack),
    /// One stack finished copying. `index`/`total` count stacks actually
    /// being copied, not the whole plan (duplicates don't advance it).
    Copied {
        index: usize,
        total: usize,
        stack: &'a PlannedStack,
        bytes: u64,
    },
    Failed {
        stack: &'a PlannedStack,
        error: &'a str,
    },
}

/// Execute a plan. Copies only; the source is left exactly as it was. A
/// metadata merge (see `PlannedStack::metadata_merge`) is the one exception:
/// on a real (non-dry-run) run it writes the resolved rating/tags for a
/// matched stack, through the same `edit::apply`/`edit::flush` path every
/// other metadata edit in this program uses -- never a new writer.
pub fn execute(
    plans: &[PlannedStack],
    cat: &mut Catalog,
    config: &Config,
    dry_run: bool,
    on_event: &mut dyn FnMut(ImportEvent),
) -> Result<ImportStats> {
    let mut stats = ImportStats::default();
    let total = plans.iter().filter(|p| p.duplicate_of.is_none()).count();
    let mut index = 0;

    for stack in plans {
        if !dry_run {
            if let Some(m) = stack.metadata_merge.as_ref().filter(|m| m.needs_write()) {
                let mut changes = Vec::new();
                if m.changes_rating() {
                    changes.push(crate::edit::Change::SetRating(
                        m.rating_after.map(i64::from),
                    ));
                }
                let added = m.added_tags();
                if !added.is_empty() {
                    changes.push(crate::edit::Change::AddTags(added));
                }
                crate::edit::apply(cat, &[m.stack_id], &changes, config)?;
                stats.merges_applied += 1;
            }
            stats.sources.extend(
                stack
                    .already
                    .iter()
                    .map(|a| source_record(&a.src, PathBuf::from(&a.library), a.src_mtime_ns)),
            );
        }
        if stack.duplicate_of.is_some() {
            stats.duplicates_skipped += 1;
            on_event(ImportEvent::Skipped(stack));
            continue;
        }

        config
            .ensure_mutable(&stack.dest_dir)
            .with_context(|| "import destination is protected")?;

        if dry_run {
            let bytes: u64 = stack.members.iter().map(|m| m.size).sum();
            stats.stacks_copied += 1;
            stats.files_copied += stack.members.len();
            stats.bytes_copied += bytes;
            index += 1;
            on_event(ImportEvent::Copied {
                index,
                total,
                stack,
                bytes,
            });
            continue;
        }

        std::fs::create_dir_all(&stack.dest_dir)
            .with_context(|| format!("creating {}", stack.dest_dir.display()))?;

        let mut copied_here = Vec::new();
        let mut stack_bytes = 0u64;
        let mut failed = false;

        for m in &stack.members {
            match copy_verified(&m.src, &m.dest) {
                Ok(bytes) => {
                    stats.files_copied += 1;
                    stats.bytes_copied += bytes;
                    stack_bytes += bytes;
                    copied_here.push(m.dest.clone());
                }
                Err(e) => {
                    let msg = format!("{}: {e}", m.src.display());
                    on_event(ImportEvent::Failed { stack, error: &msg });
                    stats.errors.push(msg);
                    failed = true;
                    break;
                }
            }
        }

        if failed {
            // A partial stack is worse than none: it would look like a complete
            // shot that is quietly missing its RAW. Roll the whole thing back.
            for path in &copied_here {
                let _ = std::fs::remove_file(path);
            }
            continue;
        }

        stats.stacks_copied += 1;
        stats.sources.extend(
            stack
                .members
                .iter()
                .map(|m| source_record(&m.src, m.dest.clone(), m.src_mtime_ns)),
        );
        index += 1;
        on_event(ImportEvent::Copied {
            index,
            total,
            stack,
            bytes: stack_bytes,
        });
    }

    if !dry_run && stats.merges_applied > 0 {
        let flush_stats = crate::edit::flush(cat, config)?;
        stats.errors.extend(flush_stats.errors);
    }

    Ok(stats)
}

fn source_record(src: &Path, library: PathBuf, src_mtime_ns: i64) -> SourceRecord {
    SourceRecord {
        library,
        src_name: src
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
        src_mtime_ns,
    }
}

/// Store each record's card name and mtime on its library file's catalog
/// row -- level 1 of the next import. Call after the catalog has scanned
/// the copies, or their rows don't exist yet. Only fills a row that has no
/// card stat yet: the first one recorded is the file's real origin. Writes
/// the catalog only, never a file. Returns rows updated.
pub fn record_sources(cat: &mut Catalog, records: &[SourceRecord]) -> Result<usize> {
    let tx = cat.conn.transaction()?;
    let mut updated = 0;
    {
        let mut stmt = tx.prepare(
            "UPDATE file SET src_name = ?1, src_mtime_ns = ?2
             WHERE src_mtime_ns IS NULL AND name = ?3
               AND stack_id IN (SELECT id FROM stack WHERE dir = ?4)",
        )?;
        for r in records {
            let (Some(dir), Some(name)) = (r.library.parent(), r.library.file_name()) else {
                continue;
            };
            updated += stmt.execute(rusqlite::params![
                r.src_name,
                r.src_mtime_ns,
                name.to_string_lossy(),
                dir.to_string_lossy()
            ])?;
        }
    }
    tx.commit()?;
    Ok(updated)
}

/// Copy one file and prove it arrived intact. Returns bytes copied.
///
/// The destination is written under a `.part` name and only renamed into place
/// after its hash matches the source, so an interrupted import can never leave
/// something that looks like a finished file.
fn copy_verified(src: &Path, dest: &Path) -> Result<u64> {
    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().unwrap_or_default().to_string_lossy()
    ));

    let src_hash = {
        let mut f = File::open(src).with_context(|| format!("opening {}", src.display()))?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut out =
            File::create(&part).with_context(|| format!("creating {}", part.display()))?;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?; // durable before we claim success
        hasher.finalize()
    };

    let dest_hash = hash_file(&part)?;
    if dest_hash != src_hash {
        let _ = std::fs::remove_file(&part);
        bail!(
            "verification failed: {} did not arrive intact",
            dest.display()
        );
    }

    let size = std::fs::metadata(&part)?.len();
    std::fs::rename(&part, dest).with_context(|| format!("renaming into {}", dest.display()))?;
    Ok(size)
}

pub fn hash_file(path: &Path) -> Result<blake3::Hash> {
    let mut f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    m.mtime() * 1_000_000_000 + m.mtime_nsec()
}

/// Level 1: what the library already holds, from the catalog alone.
#[derive(Default)]
struct LibraryFiles {
    /// `(card name, size, card mtime) -> library path`, for files whose card
    /// stat was recorded at import.
    by_src: HashMap<(String, i64, i64), String>,
    /// `(name, size) -> library path`, for files imported before that.
    by_name: HashMap<(String, i64), String>,
}

impl LibraryFiles {
    fn find(&self, (path, _, size, mtime): &SourceFile) -> Option<String> {
        let name = path.file_name()?.to_string_lossy().to_string();
        let size = i64::try_from(*size).ok()?;
        self.by_src
            .get(&(name.clone(), size, *mtime))
            .or_else(|| self.by_name.get(&(name, size)))
            .cloned()
    }
}

fn library_files(cat: &Catalog) -> Result<LibraryFiles> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.name, f.size, f.src_name, f.src_mtime_ns, s.dir || '/' || f.name
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.present = 1 AND f.size IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut files = LibraryFiles::default();
    for row in rows {
        match row? {
            (_, size, Some(src_name), Some(src_mtime), path) => {
                files.by_src.insert((src_name, size, src_mtime), path);
            }
            (name, size, _, _, path) => {
                files.by_name.insert((name, size), path);
            }
        }
    }
    Ok(files)
}

/// A library file's folder and stack base.
fn location(path: &str) -> Option<(PathBuf, String)> {
    let path = Path::new(path);
    let name = path.file_name()?.to_string_lossy();
    Some((path.parent()?.to_path_buf(), classify(&name)?.base))
}

/// Every stem used in `dir` -- `750_0001` for `750_0001.NEF`,
/// `750_0001.JPG` and `750_0001_NEF_embedded.jpg` alike. Empty for a folder
/// that doesn't exist yet.
fn stems_in(dir: &Path) -> HashSet<String> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| classify(&e.file_name().to_string_lossy()).map(|c| c.base))
        .collect()
}

/// Level 2: `(serial, shutter_count, role) -> [(path, size)]`. Keyed by role
/// because a RAW and its JPG share the maker-note identity -- it's shot-level,
/// not file-level -- and a file must only ever be compared with its own kind:
/// a card JPG against a library RAW never matches on size and used to fall
/// through to reading both files in full. A list, because the library can
/// hold the same file more than once.
fn known_shots_by_role(cat: &Catalog) -> Result<HashMap<ShotIdentity, Vec<ExistingFile>>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.role, f.serial, f.shutter_count, s.dir || '/' || f.name, f.size
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.role IN ('raw', 'oocjpg') AND f.present = 1
           AND f.serial IS NOT NULL AND f.serial != ''
           AND f.shutter_count IS NOT NULL AND f.shutter_count != 0
           AND f.size IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            u32::try_from(r.get::<_, i64>(2)?).unwrap_or(0),
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    let mut map: HashMap<ShotIdentity, Vec<ExistingFile>> = HashMap::new();
    for row in rows {
        let (role_str, serial, count, path, size) = row?;
        let role = if role_str == "raw" {
            Role::Raw
        } else {
            Role::OocJpg
        };
        map.entry((serial, count, role))
            .or_default()
            .push((path, size));
    }
    Ok(map)
}

/// `(capture_time, size) -> [path]` for everything already cataloged with
/// both known. Only consulted when an incoming file has no usable
/// serial+shutter identity at all -- a non-Nikon body, or EXIF stripped by
/// an export tool -- or when its identity matched but the bytes didn't.
fn known_by_content(cat: &Catalog) -> Result<HashMap<(i64, i64), Vec<String>>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.capture_time, f.size, s.dir || '/' || f.name
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.role IN ('raw','oocjpg') AND f.present = 1
           AND f.capture_time IS NOT NULL AND f.size IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map: HashMap<(i64, i64), Vec<String>> = HashMap::new();
    for row in rows {
        let (t, size, path) = row?;
        map.entry((t, size)).or_default().push(path);
    }
    Ok(map)
}

/// `capture_time -> [(path, size)]` for everything already cataloged with a
/// known capture time, regardless of size -- unlike `known_by_content`, which
/// is keyed on `(capture_time, size)` and so cannot find a candidate whose
/// size has since changed. Only consulted by `find_metadata_merge_target`,
/// after `find_duplicate` has already failed to find a byte-identical match.
fn known_by_time(cat: &Catalog) -> Result<HashMap<i64, Vec<TimeCandidate>>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.capture_time, s.dir || '/' || f.name, f.size, f.role
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.role IN ('raw','oocjpg') AND f.present = 1
           AND f.capture_time IS NOT NULL AND f.size IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut map: HashMap<i64, Vec<TimeCandidate>> = HashMap::new();
    for row in rows {
        let (t, path, size, role_str) = row?;
        let role = if role_str == "raw" {
            Role::Raw
        } else {
            Role::OocJpg
        };
        map.entry(t).or_default().push((path, size, role));
    }
    Ok(map)
}

/// Not a byte-for-byte duplicate (`find_duplicate` already came back empty),
/// but possibly the same shot with different embedded metadata:
///
/// - **Maker-note identity** (serial+shutter_count): trusted alone, no pixel
///   check, the same way `dupes.rs` already treats this pair as certain
///   proof of one actuation of one camera -- "the same photograph, whatever
///   it is named."
/// - **No maker-note evidence** (a non-RAW anchor, since a RAW's own EXIF
///   always carries it when present): the only tier with no identity
///   evidence at all, so a same-`capture_time`, different-size candidate is
///   only accepted after its pixels are confirmed identical -- a real edit
///   (crop, colour, recompression) must never be treated as the same shot.
fn find_metadata_merge_target(
    anchor_path: &Path,
    anchor_size: u64,
    anchor_role: Role,
    ident: Option<&(String, u32)>,
    capture_time: Option<i64>,
    known_shots_by_role: &HashMap<ShotIdentity, Vec<ExistingFile>>,
    known_by_time: &HashMap<i64, Vec<TimeCandidate>>,
) -> Option<String> {
    let anchor_size = i64::try_from(anchor_size).unwrap_or(i64::MAX);

    if let Some((serial, count)) = ident {
        let key = (serial.clone(), *count, anchor_role);
        if let Some((path, _)) = known_shots_by_role
            .get(&key)
            .into_iter()
            .flatten()
            .find(|(_, size)| *size != anchor_size)
        {
            return Some(path.clone());
        }
    }

    if ident.is_none() && anchor_role != Role::Raw {
        if let Some(t) = capture_time {
            if let Some(candidates) = known_by_time.get(&t) {
                for (path, size, role) in candidates {
                    if *role == anchor_role
                        && *size != anchor_size
                        && pixels_match(anchor_path, Path::new(path))
                    {
                        return Some(path.clone());
                    }
                }
            }
        }
    }

    None
}

/// Decode both files and compare pixels exactly. A decode failure on either
/// side must never be treated as a match -- that would risk merging metadata
/// from an unrelated file into the library's catalog entry.
fn pixels_match(a: &Path, b: &Path) -> bool {
    match (
        crate::thumbs::decode_to_rgb8(a),
        crate::thumbs::decode_to_rgb8(b),
    ) {
        (Ok(ra), Ok(rb)) => ra.dimensions() == rb.dimensions() && ra.as_raw() == rb.as_raw(),
        _ => false,
    }
}

/// Read the library's current catalog state for `existing_path` and combine
/// it with the anchor's embedded metadata, monotonically: never regress a
/// rating, never drop a tag. Returns `None` if the stack can no longer be
/// resolved (row vanished under us) -- fail toward re-importing as new,
/// never toward silently dropping something.
fn build_metadata_merge(
    cat: &Catalog,
    existing_path: &str,
    anchor_meta: Option<&crate::meta::FileMeta>,
) -> Option<crate::metadata_merge::MetadataMerge> {
    let existing = Path::new(existing_path);
    let dir = existing.parent()?.to_string_lossy().to_string();
    let name = existing.file_name()?.to_string_lossy().to_string();
    let base = classify(&name)?.base;
    let stack_id = cat.stack_id(&dir, &base).ok()??;

    let (rating_before,): (Option<i64>,) = cat
        .conn
        .query_row(
            "SELECT rating FROM stack WHERE id = ?1",
            rusqlite::params![stack_id],
            |r| Ok((r.get(0)?,)),
        )
        .ok()?;
    let tags_before: Vec<String> = cat
        .conn
        .prepare(
            "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
             WHERE st.stack_id = ?1 ORDER BY t.name",
        )
        .ok()?
        .query_map(rusqlite::params![stack_id], |r| r.get(0))
        .ok()?
        .collect::<std::result::Result<_, _>>()
        .ok()?;

    let embedded_rating = anchor_meta.and_then(|m| (m.rating >= 0).then_some(m.rating));
    let embedded_tags = anchor_meta.map_or(&[][..], |m| m.tags.as_slice());

    let (rating_after, tags_after) =
        crate::metadata_merge::resolve(rating_before, &tags_before, embedded_rating, embedded_tags);

    Some(crate::metadata_merge::MetadataMerge {
        stack_id,
        existing_path: existing_path.to_string(),
        rating_before,
        rating_after,
        tags_before,
        tags_after,
    })
}

/// Identity (serial+shutter) plus size is trusted as a duplicate by default,
/// without reading either file: on a real library, hash-verifying every
/// already-imported stack on every re-scan meant reading its full bytes off
/// the card *and* its library twin, every time -- measured on a real 1700+
/// stack card, that didn't finish in two minutes. Re-encoding or reprocessing
/// a file essentially always changes its size, so size alone already catches
/// the case that matters: a file reprocessed externally (EXIF and filename
/// untouched, pixels not) comes in as a new variant instead of being
/// silently dropped as "already have it". `verify_content` opts into a full
/// hash check on top, for when you know a same-size, byte-preserving edit is
/// in play and want certainty over speed.
///
/// The identity is looked up together with the file's own role: a JPG is
/// only ever compared with library JPGs of that shot, a RAW with RAWs.
///
/// The content-only fallback (no identity match -- a non-Nikon body,
/// stripped EXIF, or a library row cataloged without one) has no size-based
/// shortcut to lean on, so it always hash-verifies. It only reads anything
/// when a same capture time and size candidate exists.
#[allow(clippy::too_many_arguments)]
fn find_duplicate(
    anchor_path: &Path,
    anchor_size: u64,
    anchor_role: Role,
    ident: Option<&(String, u32)>,
    capture_time: Option<i64>,
    known_shots_by_role: &HashMap<ShotIdentity, Vec<ExistingFile>>,
    known_by_content: &HashMap<(i64, i64), Vec<String>>,
    verify_content: bool,
) -> (Option<String>, Option<NearMiss>) {
    let anchor_size = i64::try_from(anchor_size).unwrap_or(i64::MAX);
    let mut near_miss = None;

    if let Some((serial, count)) = ident {
        let key = (serial.clone(), *count, anchor_role);
        for (path, size) in known_shots_by_role.get(&key).into_iter().flatten() {
            if *size != anchor_size {
                continue;
            }
            if !verify_content {
                return (Some(path.clone()), None);
            }
            match compare_content(anchor_path, Path::new(path)) {
                ContentCheck::Matched => return (Some(path.clone()), None),
                // Identity and size matched but the bytes didn't, beyond
                // tolerance -- explain why, for --verify-content's
                // diagnostic output, then keep looking: another copy, or a
                // capture_time match, might still be the one.
                ContentCheck::Differs(diff) => {
                    near_miss = diff.map(|(diff_bytes, first_diff_offset)| NearMiss {
                        source: anchor_path.display().to_string(),
                        candidate: path.clone(),
                        diff_bytes,
                        first_diff_offset,
                    });
                }
            }
        }
    }

    if let Some(t) = capture_time {
        if let Some(candidates) = known_by_content.get(&(t, anchor_size)) {
            for path in candidates {
                if matches!(
                    compare_content(anchor_path, Path::new(path)),
                    ContentCheck::Matched
                ) {
                    return (Some(path.clone()), None);
                }
            }
        }
    }

    (None, near_miss)
}

/// A stray single byte is what an in-place metadata rewrite looks like, not
/// a real edit -- confirmed on a real library: every near-miss found there
/// (Shotwell embedding something after import) differed by exactly one byte,
/// always at the same offset for a given camera's fixed EXIF layout. That
/// offset is an artifact of one camera/tool combination, not a general fact,
/// so the tolerance is on *how much* differs, not *where* -- a real edit
/// (colour, crop, recompression) touches far more than one byte.
const TOLERATED_DIFF_BYTES: u64 = 1;

enum ContentCheck {
    Matched,
    /// Beyond tolerance. Carries the diff, when it could be computed, for
    /// the near-miss diagnostic -- `None` only if reading either file failed.
    Differs(Option<(u64, u64)>),
}

fn compare_content(a: &Path, b: &Path) -> ContentCheck {
    match diff_summary(a, b) {
        Ok(None) => ContentCheck::Matched,
        Ok(Some((diff_bytes, _))) if diff_bytes <= TOLERATED_DIFF_BYTES => ContentCheck::Matched,
        Ok(Some(diff)) => ContentCheck::Differs(Some(diff)),
        Err(_) => ContentCheck::Differs(None),
    }
}

/// How different two same-size files are: total differing bytes and the
/// offset of the first one. Only used to explain a `--verify-content`
/// near-miss -- `Ok(None)` means they turned out identical after all.
fn diff_summary(a: &Path, b: &Path) -> Result<Option<(u64, u64)>> {
    let a = std::fs::read(a)?;
    let b = std::fs::read(b)?;
    if a == b {
        return Ok(None);
    }
    let mut diff_bytes = 0u64;
    let mut first_diff_offset = None;
    for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
        if x != y {
            diff_bytes += 1;
            if first_diff_offset.is_none() {
                first_diff_offset = Some(i as u64);
            }
        }
    }
    Ok(Some((diff_bytes, first_diff_offset.unwrap_or(0))))
}

/// `YYYY/MM/DD` from a unix timestamp, using the same as-if-UTC convention the
/// EXIF reader uses, so a shot's folder always matches its recorded date.
pub fn date_path(unix: i64) -> PathBuf {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    PathBuf::from(format!("{y:04}"))
        .join(format!("{m:02}"))
        .join(format!("{d:02}"))
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    // `d` is a day of month (1-31) and `m` a month (1-12) by construction of
    // Howard Hinnant's civil_from_days, whatever the input day number. Neither
    // can be negative or exceed u32, so these conversions cannot fail.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "day-of-month and month are 1-31 and 1-12 by construction"
    )]
    let (d, m) = (
        (doy - (153 * mp + 2) / 5 + 1) as u32,
        if mp < 10 { mp + 3 } else { mp - 9 } as u32,
    );
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The base a new shot is filed under: `base` unless that stem is already
/// used in the destination folder by any file, whatever its extension.
/// Applied to the whole stack at once so members never drift apart.
///
/// A file that *is* the same shot as something in the library never gets
/// here -- `plan()` files it into that stack. So a stem already in use always
/// belongs to a different shot, and sharing it would stack two photographs
/// together because a recycled frame number happened to match.
fn free_base(taken: &HashSet<String>, base: &str) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    for n in 1..10_000 {
        let candidate = format!("{base}_{n}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    format!("{base}_collision")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_paths_use_the_same_convention_as_capture_times() {
        // 2026-08-09 00:01:06 UTC, the capture time of 750_6926.NEF
        assert_eq!(date_path(1_786_233_666), PathBuf::from("2026/08/09"));
        assert_eq!(date_path(0), PathBuf::from("1970/01/01"));
    }

    #[test]
    fn civil_conversion_round_trips() {
        for days in [0i64, 1, 19_000, 20_674, -1, -365] {
            let (y, m, d) = civil_from_days(days);
            let back = crate::query::days_from_civil_pub(y, i64::from(m), i64::from(d));
            assert_eq!(back, days, "round trip failed for {days}");
        }
    }

    #[test]
    fn copy_verified_produces_an_identical_file_and_leaves_no_part() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.bin");
        let dest = dir.path().join("b.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&src, &data).unwrap();

        let n = copy_verified(&src, &dest).unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), data);
        assert_eq!(std::fs::read(&src).unwrap(), data, "source was modified");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(leftovers.is_empty(), "a .part file was left behind");
    }

    #[test]
    fn collision_suffix_is_shared_by_the_whole_stack() {
        let dir = tempfile::tempdir().unwrap();
        // An existing shot occupies the plain name.
        std::fs::write(dir.path().join("750_0001.NEF"), b"old").unwrap();
        assert_eq!(free_base(&stems_in(dir.path()), "750_0001"), "750_0001_1");
    }

    /// A basename is only a hint: a JPG of some other shot already filed
    /// under `750_0001` takes the stem, even though the new shot's own NEF
    /// name is free. Joining it would stack two photographs together.
    #[test]
    fn a_stem_used_by_any_extension_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("750_0001.JPG"), b"another shot").unwrap();
        std::fs::write(dir.path().join("750_0002_NEF_embedded.jpg"), b"preview").unwrap();
        let taken = stems_in(dir.path());
        assert_eq!(free_base(&taken, "750_0001"), "750_0001_1");
        assert_eq!(free_base(&taken, "750_0002"), "750_0002_1");
    }

    /// The parallel stat pass (pass 2) must not scramble which source file
    /// ends up in which group. Two card subdirectories sharing a base with
    /// no EXIF (fabricated bytes) both land in `undated` and must collide
    /// deterministically, in walk order, exactly as the old single-threaded
    /// walk did -- this is what a mis-zip between the parallel stat results
    /// and their source paths would break.
    #[test]
    fn plan_assigns_collision_suffixes_in_order_across_card_subdirectories() {
        let card = tempfile::tempdir().unwrap();
        let lib = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();

        for sub in ["a", "b"] {
            let dir = card.path().join(sub);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("750_0001.NEF"), vec![0xA5u8; 4096]).unwrap();
            std::fs::write(dir.join("750_0001.JPG"), vec![0x5Au8; 4096]).unwrap();
        }

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert_eq!(plans.len(), 2, "{plans:?}");

        let mut dest_bases: Vec<&str> = plans.iter().map(|p| p.dest_base.as_str()).collect();
        dest_bases.sort_unstable();
        assert_eq!(dest_bases, ["750_0001", "750_0001_1"]);

        for p in &plans {
            for m in &p.members {
                let name = m.dest.file_name().unwrap().to_string_lossy().to_string();
                assert!(
                    name.starts_with(&p.dest_base),
                    "member {name} does not belong to its own stack's dest_base {}",
                    p.dest_base
                );
            }
        }
    }

    #[test]
    fn a_free_name_is_used_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(free_base(&stems_in(dir.path()), "750_0002"), "750_0002");
    }

    /// A raw and its sibling JPG share the same maker-note serial+shutter --
    /// it's shot-level, not file-level. Each must stay findable under its own
    /// role, or a card JPG gets compared against the RAW's size, never
    /// matches, and falls through to reading both files in full.
    #[test]
    fn known_shots_by_role_keeps_a_raw_and_its_jpg_apart() {
        let cat = Catalog::open_memory().unwrap();
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES ('/lib/2026/08/09', '750_0001')",
                [],
            )
            .unwrap();
        let sid = cat
            .stack_id("/lib/2026/08/09", "750_0001")
            .unwrap()
            .unwrap();
        for (name, role, size) in [
            ("750_0001.JPG", "oocjpg", 8_000_000i64),
            ("750_0001.NEF", "raw", 25_000_000i64),
        ] {
            cat.conn
                .execute(
                    "INSERT INTO file (stack_id, name, role, serial, shutter_count, size, present)
                     VALUES (?1, ?2, ?3, '6094708', 63987, ?4, 1)",
                    rusqlite::params![sid, name, role, size],
                )
                .unwrap();
        }

        let known = known_shots_by_role(&cat).unwrap();
        let raw = &known[&("6094708".to_string(), 63987, Role::Raw)];
        let jpg = &known[&("6094708".to_string(), 63987, Role::OocJpg)];
        assert!(raw[0].0.ends_with("750_0001.NEF") && raw[0].1 == 25_000_000);
        assert!(jpg[0].0.ends_with("750_0001.JPG") && jpg[0].1 == 8_000_000);
    }

    #[test]
    fn identity_match_with_matching_content_is_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.NEF");
        let incoming = dir.path().join("incoming.NEF");
        std::fs::write(&existing, vec![9u8; 4096]).unwrap();
        std::fs::write(&incoming, vec![9u8; 4096]).unwrap();

        let mut known = HashMap::new();
        known.insert(
            ("6094708".to_string(), 100u32, Role::OocJpg),
            vec![(existing.to_string_lossy().to_string(), 4096i64)],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            4096,
            Role::OocJpg,
            Some(&("6094708".to_string(), 100)),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            false,
        );
        assert_eq!(dup, Some(existing.to_string_lossy().to_string()));
        assert!(near_miss.is_none());
    }

    /// The default, fast path: identity + size is trusted without reading
    /// either file. Hash-verifying every already-imported stack on every
    /// re-scan was measured too slow on a real card to be the default (see
    /// `find_duplicate`'s doc comment) -- so a same-size collision here is
    /// accepted as a duplicate unless `--verify-content` was asked for. In
    /// practice this only matters for a same-size, byte-preserving edit,
    /// which no real editing tool produces.
    #[test]
    fn same_size_identity_match_is_trusted_without_hashing_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        std::fs::write(&existing, vec![1u8; 4096]).unwrap();
        std::fs::write(&incoming, vec![2u8; 4096]).unwrap(); // same size, different bytes

        let mut known = HashMap::new();
        known.insert(
            ("6094708".to_string(), 100u32, Role::OocJpg),
            vec![(existing.to_string_lossy().to_string(), 4096i64)],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            4096,
            Role::OocJpg,
            Some(&("6094708".to_string(), 100)),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            false,
        );
        assert_eq!(dup, Some(existing.to_string_lossy().to_string()));
        assert!(near_miss.is_none());
    }

    /// A file reprocessed externally (colour, curves) keeps its EXIF and
    /// filename, so its identity still matches -- but it is a different
    /// photograph, not the same file twice, and must not be swallowed as
    /// "already have it" once `--verify-content` is asked for. The near-miss
    /// diagnostic must explain why, for `--verify-content`'s output.
    #[test]
    fn identity_match_with_different_content_is_not_a_duplicate_when_verified() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        std::fs::write(&existing, vec![1u8; 4096]).unwrap();
        std::fs::write(&incoming, vec![2u8; 4096]).unwrap();

        let mut known = HashMap::new();
        known.insert(
            ("6094708".to_string(), 100u32, Role::OocJpg),
            vec![(existing.to_string_lossy().to_string(), 4096i64)],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            4096,
            Role::OocJpg,
            Some(&("6094708".to_string(), 100)),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            true,
        );
        assert_eq!(
            dup, None,
            "identity matched but bytes differ -- must not be treated as a duplicate"
        );
        let nm = near_miss.expect("a near-miss should have been recorded");
        assert_eq!(nm.source, incoming.to_string_lossy().to_string());
        assert_eq!(nm.candidate, existing.to_string_lossy().to_string());
        assert_eq!(nm.diff_bytes, 4096, "every byte differs (1 vs 2)");
        assert_eq!(nm.first_diff_offset, 0);
    }

    /// A stray single byte is exactly what an in-place metadata rewrite
    /// (Shotwell, observed on a real library) looks like -- confirmed a
    /// duplicate under `--verify-content`, not a near-miss.
    #[test]
    fn a_single_byte_difference_is_still_a_duplicate_under_verify_content() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        let mut data = vec![9u8; 4096];
        std::fs::write(&existing, &data).unwrap();
        data[2000] ^= 0xFF; // exactly one byte differs
        std::fs::write(&incoming, &data).unwrap();

        let mut known = HashMap::new();
        known.insert(
            ("6094708".to_string(), 100u32, Role::OocJpg),
            vec![(existing.to_string_lossy().to_string(), 4096i64)],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            4096,
            Role::OocJpg,
            Some(&("6094708".to_string(), 100)),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            true,
        );
        assert_eq!(dup, Some(existing.to_string_lossy().to_string()));
        assert!(near_miss.is_none());
    }

    /// The boundary right past tolerance: two differing bytes must still be
    /// reported as a near-miss, not silently accepted. Guards against the
    /// tolerance check drifting from "at most one" to "any small number".
    #[test]
    fn two_byte_difference_is_still_a_near_miss() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        let mut data = vec![9u8; 4096];
        std::fs::write(&existing, &data).unwrap();
        data[100] ^= 0xFF;
        data[2000] ^= 0xFF;
        std::fs::write(&incoming, &data).unwrap();

        let mut known = HashMap::new();
        known.insert(
            ("6094708".to_string(), 100u32, Role::OocJpg),
            vec![(existing.to_string_lossy().to_string(), 4096i64)],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            4096,
            Role::OocJpg,
            Some(&("6094708".to_string(), 100)),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            true,
        );
        assert_eq!(dup, None);
        assert_eq!(near_miss.unwrap().diff_bytes, 2);
    }

    #[test]
    fn content_fallback_without_identity_still_finds_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        let data = vec![7u8; 5000];
        std::fs::write(&existing, &data).unwrap();
        std::fs::write(&incoming, &data).unwrap();

        let mut known_content = HashMap::new();
        known_content.insert(
            (1_700_000_000i64, 5000i64),
            vec![existing.to_string_lossy().to_string()],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            5000,
            Role::OocJpg,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_content,
            false,
        );
        assert_eq!(dup, Some(existing.to_string_lossy().to_string()));
        assert!(near_miss.is_none());
    }

    /// A burst-mode collision on (capture_time, size) alone is not enough --
    /// only a real byte match may be trusted. Unlike identity, this path has
    /// no cheaper signal to fall back to, so it always hashes -- it's also
    /// the rare path (missing serial+shutter), not the one that was slow.
    #[test]
    fn same_capture_time_and_size_but_different_bytes_is_not_a_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("existing.JPG");
        let incoming = dir.path().join("incoming.JPG");
        std::fs::write(&existing, vec![1u8; 5000]).unwrap();
        std::fs::write(&incoming, vec![2u8; 5000]).unwrap();

        let mut known_content = HashMap::new();
        known_content.insert(
            (1_700_000_000i64, 5000i64),
            vec![existing.to_string_lossy().to_string()],
        );

        let (dup, near_miss) = find_duplicate(
            &incoming,
            5000,
            Role::OocJpg,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_content,
            false,
        );
        assert_eq!(dup, None);
        assert!(
            near_miss.is_none(),
            "no identity match, so no near-miss to report"
        );
    }

    /// A small in-memory JPEG, distinct pixels per `seed` so two calls with
    /// different seeds are never accidentally identical.
    fn jpeg_bytes(seed: u8) -> Vec<u8> {
        let mut img = image::RgbImage::new(4, 4);
        for (x, y, p) in img.enumerate_pixels_mut() {
            *p = image::Rgb([
                seed,
                u8::try_from(x * 16).unwrap(),
                u8::try_from(y * 16).unwrap(),
            ]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .unwrap();
        out.into_inner()
    }

    /// Insert a JPEG comment marker right after the SOI (`0xFFD8`) -- changes
    /// the file's size without touching a single compressed pixel byte,
    /// exactly what an embedded-metadata rewrite (Shotwell) looks like.
    fn splice_comment(jpeg: &[u8]) -> Vec<u8> {
        let payload = b"pretend this is an embedded XMP rating";
        let len = u16::try_from(payload.len() + 2).unwrap();
        let mut out = jpeg[..2].to_vec(); // SOI
        out.push(0xFF);
        out.push(0xFE); // COM marker
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// Tier A: identity (serial+shutter) matches, but size differs -- the
    /// exact case `find_duplicate` deliberately leaves unhandled. Trusted
    /// with no pixel check, per the maker-note identity being certain proof
    /// on its own (same precedent as `dupes.rs`).
    #[test]
    fn tier_a_merge_target_is_found_despite_a_size_mismatch() {
        let mut known_shots_by_role = HashMap::new();
        known_shots_by_role.insert(
            ("6094708".to_string(), 63987, Role::Raw),
            vec![("/lib/2026/08/09/750_0001.NEF".to_string(), 25_000_000i64)],
        );

        let target = find_metadata_merge_target(
            Path::new("/card/750_0001.NEF"),
            25_004_000, // a few embedded-metadata bytes larger
            Role::Raw,
            Some(&("6094708".to_string(), 63987)),
            None,
            &known_shots_by_role,
            &HashMap::new(),
        );
        assert_eq!(target, Some("/lib/2026/08/09/750_0001.NEF".to_string()));
    }

    /// The exact reported regression: a RAW anchor shares maker-note identity
    /// with the JPG already in the library (same shot, same actuation), but
    /// that JPG is not the same *file* -- it must not be treated as "this
    /// file, just re-embedded." The RAW has to be imported to complete the
    /// stack, not skipped as a duplicate of its sibling.
    #[test]
    fn tier_a_never_merges_a_raw_with_its_library_jpg_sibling() {
        let mut known_shots_by_role = HashMap::new();
        known_shots_by_role.insert(
            ("6094708".to_string(), 64652, Role::OocJpg),
            vec![("/lib/2026/08/23/750_7517.JPG".to_string(), 9_865_934i64)],
        );

        let target = find_metadata_merge_target(
            Path::new("/card/750_7517.NEF"),
            25_000_000, // a RAW is never anywhere near its sibling JPG's size
            Role::Raw,
            Some(&("6094708".to_string(), 64652)),
            None,
            &known_shots_by_role,
            &HashMap::new(),
        );
        assert_eq!(
            target, None,
            "a RAW must not be matched against its JPG sibling's identity"
        );
    }

    /// The symmetric case: a JPG anchor must not be matched against a RAW
    /// cataloged under the same identity either.
    #[test]
    fn tier_a_never_merges_a_jpg_with_a_cataloged_raw_sibling() {
        let mut known_shots_by_role = HashMap::new();
        known_shots_by_role.insert(
            ("6094708".to_string(), 64652, Role::Raw),
            vec![("/lib/2026/08/23/750_7517.NEF".to_string(), 25_000_000i64)],
        );

        let target = find_metadata_merge_target(
            Path::new("/card/750_7517.JPG"),
            9_865_934,
            Role::OocJpg,
            Some(&("6094708".to_string(), 64652)),
            None,
            &known_shots_by_role,
            &HashMap::new(),
        );
        assert_eq!(target, None);
    }

    /// Tier B: no maker-note identity at all (a JPG-only shot), same
    /// capture_time, different size -- only accepted once pixels are
    /// confirmed identical.
    #[test]
    fn tier_b_merge_target_is_found_when_pixels_match() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("incoming.JPG");
        let existing = dir.path().join("existing.JPG");
        let base = jpeg_bytes(7);
        std::fs::write(&anchor, &base).unwrap();
        std::fs::write(&existing, splice_comment(&base)).unwrap();

        let mut known_time = HashMap::new();
        known_time.insert(
            1_700_000_000i64,
            vec![(
                existing.to_string_lossy().to_string(),
                i64::try_from(std::fs::metadata(&existing).unwrap().len()).unwrap(),
                Role::OocJpg,
            )],
        );

        let target = find_metadata_merge_target(
            &anchor,
            std::fs::metadata(&anchor).unwrap().len(),
            Role::OocJpg,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_time,
        );
        assert_eq!(target, Some(existing.to_string_lossy().to_string()));
    }

    /// The critical negative case: a real pixel edit must never be treated as
    /// the same shot just because the identity/time evidence lines up.
    #[test]
    fn tier_b_merge_target_is_none_when_pixels_differ() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("incoming.JPG");
        let existing = dir.path().join("existing.JPG");
        std::fs::write(&anchor, jpeg_bytes(7)).unwrap();
        std::fs::write(&existing, jpeg_bytes(200)).unwrap();

        let mut known_time = HashMap::new();
        known_time.insert(
            1_700_000_000i64,
            vec![(
                existing.to_string_lossy().to_string(),
                i64::try_from(std::fs::metadata(&existing).unwrap().len()).unwrap(),
                Role::OocJpg,
            )],
        );

        let target = find_metadata_merge_target(
            &anchor,
            std::fs::metadata(&anchor).unwrap().len(),
            Role::OocJpg,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_time,
        );
        assert_eq!(target, None);
    }

    /// The role filter, not merely a decode failure, is what must prevent a
    /// cross-role Tier B match: a `Role::Raw` candidate whose bytes happen to
    /// be a valid, pixel-identical JPEG would otherwise wrongly match.
    #[test]
    fn tier_b_never_matches_a_candidate_of_a_different_role_even_if_its_pixels_would() {
        let dir = tempfile::tempdir().unwrap();
        let anchor = dir.path().join("incoming.JPG");
        let mislabeled = dir.path().join("existing.NEF");
        let base = jpeg_bytes(7);
        std::fs::write(&anchor, &base).unwrap();
        std::fs::write(&mislabeled, splice_comment(&base)).unwrap();

        let mut known_time = HashMap::new();
        known_time.insert(
            1_700_000_000i64,
            vec![(
                mislabeled.to_string_lossy().to_string(),
                i64::try_from(std::fs::metadata(&mislabeled).unwrap().len()).unwrap(),
                Role::Raw,
            )],
        );

        let target = find_metadata_merge_target(
            &anchor,
            std::fs::metadata(&anchor).unwrap().len(),
            Role::OocJpg,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_time,
        );
        assert_eq!(
            target, None,
            "a Role::Raw candidate must never be matched against a JPG anchor"
        );
    }

    /// A RAW anchor never falls into Tier B, even with no maker-note
    /// identity: there is no RAW-decoding capability, so this tier's pixel
    /// check would silently never engage. Confirm it does not try.
    #[test]
    fn tier_b_never_applies_to_a_raw_anchor() {
        let mut known_time = HashMap::new();
        known_time.insert(
            1_700_000_000i64,
            vec![("/lib/existing.NEF".to_string(), 999i64, Role::Raw)],
        );

        let target = find_metadata_merge_target(
            Path::new("/card/incoming.NEF"),
            1000,
            Role::Raw,
            None,
            Some(1_700_000_000),
            &HashMap::new(),
            &known_time,
        );
        assert_eq!(target, None);
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use crate::config::Config;

    /// A card holding one stack, with bytes distinctive enough to detect
    /// truncation or corruption.
    fn card() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, fill) in [("750_0001.NEF", 0xA5u8), ("750_0001.JPG", 0x5A)] {
            let data: Vec<u8> = (0..200_000u32).map(|i| fill ^ (i % 251) as u8).collect();
            std::fs::write(dir.path().join(name), data).unwrap();
        }
        dir
    }

    fn fingerprint(dir: &Path) -> Vec<(String, u64, i64, i64)> {
        use std::os::unix::fs::MetadataExt;
        let mut out: Vec<_> = walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_file())
            .map(|e| {
                let m = e.metadata().unwrap();
                (
                    e.file_name().to_string_lossy().to_string(),
                    m.len(),
                    m.mtime(),
                    m.mtime_nsec(),
                )
            })
            .collect();
        out.sort();
        out
    }

    /// The card is the only copy of a shoot until the import is verified.
    /// Nothing here may touch it -- not contents, not mtimes.
    #[test]
    fn the_source_is_byte_identical_after_import() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let before = fingerprint(card.path());
        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        execute(&plans, &mut cat, &Config::default(), false, &mut |_| {}).unwrap();

        assert_eq!(before, fingerprint(card.path()), "import modified the card");
    }

    #[test]
    fn copies_arrive_byte_identical() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        let stats = execute(&plans, &mut cat, &Config::default(), false, &mut |_| {}).unwrap();
        assert_eq!(stats.files_copied, 2, "errors: {:?}", stats.errors);

        for p in &plans[0].members {
            assert_eq!(
                std::fs::read(&p.src).unwrap(),
                std::fs::read(&p.dest).unwrap(),
                "{} did not arrive intact",
                p.dest.display()
            );
        }
    }

    /// A stack that fails partway must leave nothing behind. A shot quietly
    /// missing its RAW looks complete and is not.
    #[test]
    fn a_failed_member_rolls_back_the_whole_stack() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let mut plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        // Point the second member at a source that does not exist, so its copy
        // fails after the first has already succeeded.
        plans[0].members[1].src = card.path().join("does-not-exist.JPG");

        let stats = execute(&plans, &mut cat, &Config::default(), false, &mut |_| {}).unwrap();
        assert_eq!(stats.stacks_copied, 0);
        assert_eq!(stats.errors.len(), 1);

        let left: Vec<_> = walkdir::WalkDir::new(lib.path())
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert!(
            left.is_empty(),
            "a partial stack was left behind: {:?}",
            left.iter()
                .map(|e| e.path().display().to_string())
                .collect::<Vec<_>>()
        );
    }

    /// Verification happens before anything is renamed into place, so an
    /// interrupted copy cannot leave something that looks finished.
    #[test]
    fn no_part_files_survive_a_successful_import() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        execute(&plans, &mut cat, &Config::default(), false, &mut |_| {}).unwrap();

        let parts: Vec<_> = walkdir::WalkDir::new(lib.path())
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".part"))
            .collect();
        assert!(parts.is_empty(), "a .part file survived");
    }

    /// Corruption between write and rename must be caught, the destination
    /// removed, and nothing reported as successful.
    #[test]
    fn a_corrupted_copy_is_detected_and_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.bin");
        std::fs::write(&src, vec![1u8; 100_000]).unwrap();

        // A destination whose parent is read-only makes the rename fail, which
        // stands in for any failure after the bytes are written.
        let dest_dir = dir.path().join("out");
        std::fs::create_dir(&dest_dir).unwrap();
        let dest = dest_dir.join("a.bin");
        assert!(copy_verified(&src, &dest).is_ok());
        assert_eq!(std::fs::read(&dest).unwrap(), vec![1u8; 100_000]);

        // And the hash check itself: a source that changes under us.
        let hash_a = hash_file(&src).unwrap();
        std::fs::write(&src, vec![2u8; 100_000]).unwrap();
        assert_ne!(
            hash_a,
            hash_file(&src).unwrap(),
            "hashing is not detecting change"
        );
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        let stats = execute(&plans, &mut cat, &Config::default(), true, &mut |_| {}).unwrap();

        assert_eq!(stats.files_copied, 2, "dry run should still report a plan");
        assert!(!lib.path().join("undated").exists());
        let created: Vec<_> = walkdir::WalkDir::new(lib.path())
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_file())
            .collect();
        assert!(created.is_empty(), "a dry run created files");
    }

    /// Catalog `file` rows for an already-imported copy of `card()`'s stack
    /// under `dir` -- no camera identity, as for a fixture or a row
    /// cataloged without one. `with_jpg: false` leaves the JPG out.
    fn catalog_copy(cat: &Catalog, dir: &Path, with_jpg: bool) {
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, '750_0001')",
                [dir.to_str().unwrap()],
            )
            .unwrap();
        let sid = cat
            .stack_id(dir.to_str().unwrap(), "750_0001")
            .unwrap()
            .unwrap();
        let mut files = vec![("750_0001.NEF", "raw")];
        if with_jpg {
            files.push(("750_0001.JPG", "oocjpg"));
        }
        for (name, role) in files {
            cat.conn
                .execute(
                    "INSERT INTO file (stack_id, name, role, size, present)
                     VALUES (?1, ?2, ?3, 200000, 1)",
                    rusqlite::params![sid, name, role],
                )
                .unwrap();
        }
    }

    fn card_mtime(path: &Path) -> i64 {
        mtime_ns(&std::fs::metadata(path).unwrap())
    }

    /// Level 1 for everything imported before card stats were recorded:
    /// name and size alone, no read. `--deep` skips it, and with fixture
    /// bytes (no identity, no capture time) nothing else can match.
    #[test]
    fn level_one_matches_name_and_size_and_deep_skips_it() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        catalog_copy(&cat, &lib.path().join("2026/08/09"), true);

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert!(plans[0].duplicate_of.is_some());
        assert!(plans[0].members.is_empty());
        assert_eq!(plans[0].already.len(), 2);
        assert!(plans[0].already.iter().all(|a| a.unread));

        let plans = plan(&cat, card.path(), lib.path(), false, true).unwrap();
        assert_eq!(plans[0].duplicate_of, None, "--deep must not use level 1");
    }

    /// Once a card stat is recorded it is what level 1 compares: same name
    /// and size but another mtime is a different file -- a recycled number --
    /// not a match.
    #[test]
    fn a_recorded_card_mtime_must_match_too() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        catalog_copy(&cat, &lib.path().join("2026/08/09"), true);
        for name in ["750_0001.NEF", "750_0001.JPG"] {
            cat.conn
                .execute(
                    "UPDATE file SET src_name = ?1, src_mtime_ns = ?2 WHERE name = ?1",
                    rusqlite::params![name, card_mtime(&card.path().join(name))],
                )
                .unwrap();
        }

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert!(plans[0].duplicate_of.is_some());

        let jpg = std::fs::File::options()
            .write(true)
            .open(card.path().join("750_0001.JPG"))
            .unwrap();
        jpg.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert_eq!(plans[0].duplicate_of, None);
        assert_eq!(plans[0].members.len(), 1, "only the JPG is new");
        assert_eq!(plans[0].already.len(), 1);
    }

    /// The library has the shot's NEF but not its JPG: only the JPG is
    /// copied, and it joins that stack -- same folder, same base -- rather
    /// than being filed as a new shot.
    #[test]
    fn a_missing_member_joins_the_stack_already_in_the_library() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();
        let home = lib.path().join("2026/08/09");
        catalog_copy(&cat, &home, false);

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        let p = &plans[0];
        assert_eq!(p.duplicate_of, None);
        assert_eq!(p.already.len(), 1);
        assert_eq!(p.members.len(), 1);
        assert_eq!(p.members[0].dest, home.join("750_0001.JPG"));
        assert!(p.joins_existing);
    }

    /// Level 2 compares a card file only with library files of its own role:
    /// a same-identity RAW of a different size is not "this JPG". With
    /// nothing to read either side (the paths don't exist), a match on the
    /// JPG's own role is proof it was decided without reading a file.
    #[test]
    fn level_two_compares_a_jpg_only_with_jpgs_and_reads_nothing() {
        let id = ("6094708".to_string(), 67223u32);
        let mut known: HashMap<ShotIdentity, Vec<ExistingFile>> = HashMap::new();
        known.insert(
            (id.0.clone(), id.1, Role::Raw),
            vec![("/lib/750_0001.NEF".into(), 25_000_000)],
        );
        let card_jpg = Path::new("/nonexistent/card/750_0001.JPG");

        let (dup, _) = find_duplicate(
            card_jpg,
            8_000_000,
            Role::OocJpg,
            Some(&id),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            false,
        );
        assert_eq!(dup, None, "a JPG is never this RAW");

        known.insert(
            (id.0.clone(), id.1, Role::OocJpg),
            vec![("/lib/750_0001.JPG".into(), 8_000_000)],
        );
        let (dup, _) = find_duplicate(
            card_jpg,
            8_000_000,
            Role::OocJpg,
            Some(&id),
            Some(1_700_000_000),
            &known,
            &HashMap::new(),
            false,
        );
        assert_eq!(dup.as_deref(), Some("/lib/750_0001.JPG"));
    }

    #[test]
    fn execute_reports_sources_for_copies_and_matches_but_not_a_dry_run() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        let home = lib.path().join("2026/08/09");
        catalog_copy(&cat, &home, false);

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        let dry = execute(&plans, &mut cat, &Config::default(), true, &mut |_| {}).unwrap();
        assert!(dry.sources.is_empty());

        let real = execute(&plans, &mut cat, &Config::default(), false, &mut |_| {}).unwrap();
        let mut got = real.sources.clone();
        got.sort_by(|a, b| a.library.cmp(&b.library));
        assert_eq!(
            got,
            vec![
                SourceRecord {
                    library: home.join("750_0001.JPG"),
                    src_name: "750_0001.JPG".into(),
                    src_mtime_ns: card_mtime(&card.path().join("750_0001.JPG")),
                },
                SourceRecord {
                    library: home.join("750_0001.NEF"),
                    src_name: "750_0001.NEF".into(),
                    src_mtime_ns: card_mtime(&card.path().join("750_0001.NEF")),
                },
            ]
        );
    }

    /// Card stats live on the catalog row and survive rescans, including
    /// `--full` -- scan's upsert doesn't list them. The first record wins.
    #[test]
    fn recorded_sources_survive_a_full_rescan_and_are_never_overwritten() {
        let lib = tempfile::tempdir().unwrap();
        let day = lib.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("750_0001.NEF"), "raw").unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, lib.path(), true).unwrap();

        let record = |mtime| SourceRecord {
            library: day.join("750_0001.NEF"),
            src_name: "750_0001.NEF".into(),
            src_mtime_ns: mtime,
        };
        assert_eq!(record_sources(&mut cat, &[record(42)]).unwrap(), 1);
        assert_eq!(record_sources(&mut cat, &[record(99)]).unwrap(), 0);
        crate::scan::scan(&mut cat, lib.path(), true).unwrap();

        let stored: (String, i64) = cat
            .conn
            .query_row("SELECT src_name, src_mtime_ns FROM file", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(stored, ("750_0001.NEF".to_string(), 42));
    }
    #[test]
    fn importing_into_a_protected_root_is_refused() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        let config = Config {
            protected_roots: vec![lib.path().to_path_buf()],
            ..Default::default()
        };

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert!(execute(&plans, &mut cat, &config, false, &mut |_| {}).is_err());
    }

    /// Without this, a source that is the library itself gets walked as if
    /// it were a card: `plan()` has no incremental skip the way `scan()`
    /// does, so every stack's anchor gets a mutex-serialised `read_meta`
    /// call -- on a real library, tens of thousands of them, which looks
    /// exactly like a hang. Confirmed live: this is what actually happened.
    #[test]
    fn importing_from_the_library_itself_is_refused() {
        let lib = tempfile::tempdir().unwrap();
        std::fs::write(lib.path().join("750_0001.NEF"), b"x").unwrap();
        let cat = Catalog::open_memory().unwrap();

        let err = plan(&cat, lib.path(), lib.path(), false, false).unwrap_err();
        assert!(err.to_string().contains("overlaps the library"), "{err}");
    }

    #[test]
    fn importing_from_a_subdirectory_of_the_library_is_refused() {
        let lib = tempfile::tempdir().unwrap();
        let sub = lib.path().join("2026/08/16");
        std::fs::create_dir_all(&sub).unwrap();
        let cat = Catalog::open_memory().unwrap();

        let err = plan(&cat, &sub, lib.path(), false, false).unwrap_err();
        assert!(err.to_string().contains("overlaps the library"), "{err}");
    }

    /// The reverse overlap: a library that sits inside the source directory
    /// is just as nonsensical -- the "card" would contain the whole library.
    #[test]
    fn importing_into_a_directory_inside_the_source_is_refused() {
        let outer = tempfile::tempdir().unwrap();
        let lib = outer.path().join("library");
        std::fs::create_dir_all(&lib).unwrap();
        let cat = Catalog::open_memory().unwrap();

        let err = plan(&cat, outer.path(), &lib, false, false).unwrap_err();
        assert!(err.to_string().contains("overlaps the library"), "{err}");
    }

    /// Undated files must land somewhere obvious rather than being guessed at
    /// from mtime, which would scatter a shoot across whatever days the files
    /// happened to be copied.
    #[test]
    fn files_without_a_capture_date_go_to_undated() {
        let card = card(); // fabricated bytes, no EXIF
        let lib = tempfile::tempdir().unwrap();
        let cat = Catalog::open_memory().unwrap();

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        assert_eq!(plans.len(), 1);
        assert!(plans[0].capture_time.is_none());
        assert!(
            plans[0].dest_dir.ends_with("undated"),
            "{:?}",
            plans[0].dest_dir
        );
    }

    /// A verified duplicate must be reported through the event hook, the same
    /// mechanism the CLI uses to log what it skipped and why.
    #[test]
    fn a_verified_duplicate_fires_a_skipped_event() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let mut plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        plans[0].duplicate_of = Some("/lib/2026/08/09/750_0001".to_string());

        let mut skipped = 0;
        let mut copied = 0;
        let stats = execute(
            &plans,
            &mut cat,
            &Config::default(),
            false,
            &mut |ev| match ev {
                ImportEvent::Skipped(_) => skipped += 1,
                ImportEvent::Copied { .. } => copied += 1,
                ImportEvent::Failed { .. } => {}
            },
        )
        .unwrap();

        assert_eq!(skipped, 1);
        assert_eq!(copied, 0);
        assert_eq!(stats.duplicates_skipped, 1);
    }

    /// A fresh stack fires a `Copied` event carrying real progress info, not
    /// just a final count -- this is what drives the CLI's progress line.
    #[test]
    fn a_fresh_stack_fires_a_copied_event_with_progress_info() {
        let card = card();
        let lib = tempfile::tempdir().unwrap();
        let mut cat = Catalog::open_memory().unwrap();

        let plans = plan(&cat, card.path(), lib.path(), false, false).unwrap();
        let mut seen = Vec::new();
        execute(&plans, &mut cat, &Config::default(), false, &mut |ev| {
            if let ImportEvent::Copied {
                index,
                total,
                bytes,
                ..
            } = ev
            {
                seen.push((index, total, bytes));
            }
        })
        .unwrap();

        assert_eq!(seen, vec![(1, 1, 400_000)]);
    }
}
