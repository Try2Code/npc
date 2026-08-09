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
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct PlannedStack {
    pub base: String,
    /// Basename actually used at the destination, with a collision suffix if
    /// one was needed. Shared by every member.
    pub dest_base: String,
    pub dest_dir: PathBuf,
    pub capture_time: Option<i64>,
    pub members: Vec<PlannedFile>,
    /// Set when this shot is already in the library.
    pub duplicate_of: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PlannedFile {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub role: Role,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub stacks_copied: usize,
    pub files_copied: usize,
    pub bytes_copied: u64,
    pub duplicates_skipped: usize,
    pub errors: Vec<String>,
}

/// Build the plan without touching anything.
pub fn plan(cat: &Catalog, source: &Path, library: &Path) -> Result<Vec<PlannedStack>> {
    if !source.is_dir() {
        bail!("import source {} is not a directory", source.display());
    }

    // Group source files into stacks, exactly as the scanner does.
    let mut groups: HashMap<(PathBuf, String), Vec<(PathBuf, Role, u64)>> = HashMap::new();
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
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        groups
            .entry((dir, c.base))
            .or_default()
            .push((entry.path().to_path_buf(), c.role, size));
    }

    // Existing (serial, shutter_count) pairs, for exact duplicate detection.
    let known = known_shots(cat)?;

    let mut plans = Vec::new();
    let mut claimed: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();

    let mut keys: Vec<_> = groups.keys().cloned().collect();
    keys.sort();

    for key in keys {
        let members = &groups[&key];
        if !members.iter().any(|(_, role, _)| role.is_image()) {
            continue; // orphan profile on the card
        }

        // Prefer the RAW for capture time and identity; fall back to any image.
        let anchor = members
            .iter()
            .find(|(_, r, _)| *r == Role::Raw)
            .or_else(|| members.iter().find(|(_, r, _)| r.is_image()));
        let (capture_time, ident) = match anchor {
            Some((path, _, _)) => match crate::meta::read_meta(&path.to_string_lossy()) {
                Ok(m) => (
                    (m.capture_time != 0).then_some(m.capture_time),
                    (!m.serial.is_empty() && m.shutter_count != 0)
                        .then(|| (m.serial.clone(), m.shutter_count)),
                ),
                Err(_) => (None, None),
            },
            None => (None, None),
        };

        let dest_dir = match capture_time {
            Some(t) => library.join(date_path(t)),
            // Undated files must not be silently scattered; they go somewhere
            // obvious rather than being guessed at from mtime.
            None => library.join("undated"),
        };

        let duplicate_of = ident.as_ref().and_then(|k| known.get(k).cloned());

        // Resolve one collision suffix for the whole stack, so members keep
        // matching basenames and their shared sidecar still works.
        let base = key.1.clone();
        let dest_base = if duplicate_of.is_some() {
            base.clone()
        } else {
            free_base(&dest_dir, &base, members, &claimed)
        };

        let planned: Vec<PlannedFile> = members
            .iter()
            .map(|(src, role, size)| {
                let name = src.file_name().unwrap_or_default().to_string_lossy();
                let suffix = name.strip_prefix(base.as_str()).unwrap_or(&name).to_string();
                let dest = dest_dir.join(format!("{dest_base}{suffix}"));
                PlannedFile {
                    src: src.clone(),
                    dest,
                    role: *role,
                    size: *size,
                }
            })
            .collect();

        for p in &planned {
            claimed.insert(p.dest.clone());
        }

        plans.push(PlannedStack {
            base,
            dest_base,
            dest_dir,
            capture_time,
            members: planned,
            duplicate_of,
        });
    }

    Ok(plans)
}

/// Execute a plan. Copies only; the source is left exactly as it was.
pub fn execute(plans: &[PlannedStack], config: &Config, dry_run: bool) -> Result<ImportStats> {
    let mut stats = ImportStats::default();

    for stack in plans {
        if stack.duplicate_of.is_some() {
            stats.duplicates_skipped += 1;
            continue;
        }

        config
            .ensure_mutable(&stack.dest_dir)
            .with_context(|| "import destination is protected")?;

        if dry_run {
            stats.stacks_copied += 1;
            stats.files_copied += stack.members.len();
            stats.bytes_copied += stack.members.iter().map(|m| m.size).sum::<u64>();
            continue;
        }

        std::fs::create_dir_all(&stack.dest_dir)
            .with_context(|| format!("creating {}", stack.dest_dir.display()))?;

        let mut copied_here = Vec::new();
        let mut failed = false;

        for m in &stack.members {
            match copy_verified(&m.src, &m.dest) {
                Ok(bytes) => {
                    stats.files_copied += 1;
                    stats.bytes_copied += bytes;
                    copied_here.push(m.dest.clone());
                }
                Err(e) => {
                    stats.errors.push(format!("{}: {e}", m.src.display()));
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
    }

    Ok(stats)
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
        let mut out = File::create(&part).with_context(|| format!("creating {}", part.display()))?;
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
    std::fs::rename(&part, dest)
        .with_context(|| format!("renaming into {}", dest.display()))?;
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

/// `(serial, shutter_count) -> "dir/base"` for everything already catalogued.
/// Exact for Nikon bodies, which is what makes import dedupe reliable rather
/// than heuristic.
fn known_shots(cat: &Catalog) -> Result<HashMap<(String, u32), String>> {
    let mut stmt = cat.conn.prepare(
        "SELECT f.serial, f.shutter_count, s.dir || '/' || s.base
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.serial IS NOT NULL AND f.serial != ''
           AND f.shutter_count IS NOT NULL AND f.shutter_count != 0",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)? as u32,
            r.get::<_, String>(2)?,
        ))
    })?;
    let mut map = HashMap::new();
    for row in rows {
        let (serial, count, path) = row?;
        map.insert((serial, count), path);
    }
    Ok(map)
}

/// `YYYY/MM/DD` from a unix timestamp, using the same as-if-UTC convention the
/// EXIF reader uses, so a shot's folder always matches its recorded date.
pub fn date_path(unix: i64) -> PathBuf {
    let (y, m, d) = civil_from_days(unix.div_euclid(86_400));
    PathBuf::from(format!("{y:04}")).join(format!("{m:02}")).join(format!("{d:02}"))
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Find a basename free for every member of the stack. Suffixing is applied to
/// the whole stack at once so members never drift apart.
fn free_base(
    dest_dir: &Path,
    base: &str,
    members: &[(PathBuf, Role, u64)],
    claimed: &std::collections::HashSet<PathBuf>,
) -> String {
    let taken = |candidate: &str| -> bool {
        members.iter().any(|(src, _, _)| {
            let name = src.file_name().unwrap_or_default().to_string_lossy();
            let suffix = name.strip_prefix(base).unwrap_or(&name);
            let p = dest_dir.join(format!("{candidate}{suffix}"));
            p.exists() || claimed.contains(&p)
        })
    };

    if !taken(base) {
        return base.to_string();
    }
    for n in 1..10_000 {
        let candidate = format!("{base}_{n}");
        if !taken(&candidate) {
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
            let back = crate::query::days_from_civil_pub(y, m as i64, d as i64);
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
        assert_eq!(n as usize, data.len());
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

        let members = vec![
            (PathBuf::from("/card/750_0001.NEF"), Role::Raw, 3),
            (PathBuf::from("/card/750_0001.JPG"), Role::OocJpg, 3),
        ];
        let chosen = free_base(dir.path(), "750_0001", &members, &Default::default());
        assert_eq!(chosen, "750_0001_1");
    }

    #[test]
    fn a_free_name_is_used_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let members = vec![(PathBuf::from("/card/750_0002.NEF"), Role::Raw, 3)];
        assert_eq!(
            free_base(dir.path(), "750_0002", &members, &Default::default()),
            "750_0002"
        );
    }
}
