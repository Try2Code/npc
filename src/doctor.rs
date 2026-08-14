//! Consistency checks.
//!
//! The catalog is a cache over files that other programs can also touch, so it
//! can be wrong in ways nothing else notices. Every drift in this project so
//! far was found by hand -- embedded ratings appearing on a protected drive,
//! extracted previews left in a library by a debugging session, rows for a
//! concept that had been removed. This is that work, automated.
//!
//! The check that matters most is the first: **originals modified behind our
//! back**. Nothing else in the program looks for it, because everything else
//! assumes an unchanged file is unchanged.

use crate::config::Config;
use crate::db::Catalog;
use anyhow::Result;
use std::path::Path;

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum Severity {
    /// Nothing to do.
    Fine,
    /// Worth knowing, not worth acting on.
    Note,
    /// Something is inconsistent and a person should decide what to do.
    Warn,
}

impl Severity {
    pub fn marker(self) -> &'static str {
        match self {
            Severity::Fine => "ok  ",
            Severity::Note => "note",
            Severity::Warn => "WARN",
        }
    }
}

#[derive(Debug)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    /// Lines of supporting detail; may be empty.
    pub detail: Vec<String>,
    /// What to do about it, when there is something.
    pub remedy: Option<String>,
}

impl Finding {
    fn fine(title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Fine,
            title: title.into(),
            detail: vec![],
            remedy: None,
        }
    }
    fn note(title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Note,
            title: title.into(),
            detail: vec![],
            remedy: None,
        }
    }
    fn warn(title: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            title: title.into(),
            detail: vec![],
            remedy: None,
        }
    }
    fn with(mut self, lines: Vec<String>) -> Self {
        self.detail = lines;
        self
    }
    fn fix(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

pub fn run(cat: &Catalog, root: &Path, config: &Config) -> Result<Vec<Finding>> {
    Ok(vec![
        check_modified_originals(cat)?,
        check_missing(cat)?,
        check_orphan_sidecars(cat)?,
        check_pending(cat)?,
        check_undated(cat)?,
        check_duplicates(cat)?,
        check_actions(),
        check_cache(root),
        check_protection(root, config),
    ])
}

/// A file whose size or mtime no longer matches what was cataloged has been
/// written by something else. That is not necessarily wrong -- RawTherapee and
/// image viewers do it -- but it is always worth knowing, because this tool's
/// whole premise is that originals do not change.
fn check_modified_originals(cat: &Catalog) -> Result<Finding> {
    use std::os::unix::fs::MetadataExt;

    let mut stmt = cat.conn.prepare(
        "SELECT s.dir || '/' || f.name, f.size, f.mtime_ns
         FROM file f JOIN stack s ON s.id = f.stack_id
         WHERE f.present = 1",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
        ))
    })?;

    let mut changed = Vec::new();
    let mut gone = 0usize;
    for row in rows {
        let (path, size, mtime) = row?;
        match std::fs::metadata(&path) {
            Ok(md) => {
                let now_mtime = md.mtime() * 1_000_000_000 + md.mtime_nsec();
                if i64::try_from(md.size()).unwrap_or(i64::MAX) != size || now_mtime != mtime {
                    changed.push(format!("{path}  ({} -> {} bytes)", size, md.size()));
                }
            }
            Err(_) => gone += 1,
        }
    }

    if changed.is_empty() {
        return Ok(Finding::fine(format!(
            "originals unchanged since cataloged{}",
            if gone > 0 {
                format!(" ({gone} not readable)")
            } else {
                String::new()
            }
        )));
    }

    let n = changed.len();
    changed.truncate(10);
    Ok(
        Finding::warn(format!("{n} original(s) modified since cataloged"))
            .with(changed)
            .fix(
                "Something other than this tool wrote to them -- an editor or a \
              viewer embedding a rating. Run `npc scan --full` to re-read \
              them, or investigate first if that is unexpected.",
            ),
    )
}

fn check_missing(cat: &Catalog) -> Result<Finding> {
    let n: i64 = cat
        .conn
        .query_row("SELECT COUNT(*) FROM file WHERE present = 0", [], |r| {
            r.get(0)
        })?;
    if n == 0 {
        return Ok(Finding::fine("no files missing"));
    }
    let sample: Vec<String> = cat
        .conn
        .prepare(
            "SELECT s.dir || '/' || f.name FROM file f JOIN stack s ON s.id = f.stack_id
             WHERE f.present = 0 LIMIT 5",
        )?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(
        Finding::note(format!("{n} file(s) recorded but not on disk"))
            .with(sample)
            .fix(
                "Kept deliberately: an unplugged drive or a renamed folder must \
              not destroy tag history. They disappear from queries but their \
              tags survive.",
            ),
    )
}

fn check_orphan_sidecars(cat: &Catalog) -> Result<Finding> {
    let n: i64 = cat
        .conn
        .query_row("SELECT COUNT(*) FROM orphan", [], |r| r.get(0))?;
    if n == 0 {
        return Ok(Finding::fine("no orphan sidecars"));
    }
    let sample: Vec<String> = cat
        .conn
        .prepare("SELECT path FROM orphan LIMIT 10")?
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(std::result::Result::ok)
        .collect();
    Ok(
        Finding::warn(format!("{n} sidecar(s) with no image beside them"))
            .with(sample)
            .fix(
                "The images they describe are gone. Safe to delete, but check \
              first -- a sidecar is the only place tags live.",
            ),
    )
}

fn check_pending(cat: &Catalog) -> Result<Finding> {
    let n: i64 = cat
        .conn
        .query_row("SELECT COUNT(*) FROM stack WHERE dirty = 1", [], |r| {
            r.get(0)
        })?;
    if n == 0 {
        return Ok(Finding::fine("no unwritten edits"));
    }
    Ok(
        Finding::warn(format!("{n} edit(s) not yet written to disk")).fix(
            "Run `npc sync` with the drive attached. Until then they exist \
              only in the catalog, which is the one place this design does not \
              want anything to exist only.",
        ),
    )
}

fn check_undated(cat: &Catalog) -> Result<Finding> {
    let n: i64 = cat.conn.query_row(
        "SELECT COUNT(*) FROM stack WHERE event_id IS NULL",
        [],
        |r| r.get(0),
    )?;
    if n == 0 {
        return Ok(Finding::fine("every stack belongs to an event"));
    }
    Ok(
        Finding::note(format!("{n} stack(s) have no capture date")).fix(
            "They are grouped under `undated`. Usually files whose EXIF was \
         stripped by an earlier edit.",
        ),
    )
}

fn check_duplicates(cat: &Catalog) -> Result<Finding> {
    let groups = crate::dupes::find(cat)?;
    if groups.is_empty() {
        return Ok(Finding::fine("no duplicates"));
    }
    let detail: Vec<String> = groups
        .iter()
        .take(5)
        .map(|g| format!("{} ({} copies)", g.paths[0], g.paths.len()))
        .collect();
    Ok(
        Finding::note(format!("{} duplicate group(s)", groups.len()))
            .with(detail)
            .fix("See `npc dupes` for the full list."),
    )
}

fn check_actions() -> Finding {
    let configured = crate::action::load();
    if configured.is_empty() {
        return Finding::warn("no actions available").fix(
            "None of the configured programs are on PATH, so the action menu \
             is empty. Check `[[action]]` entries in the config.",
        );
    }
    Finding::fine(format!(
        "{} action(s) available: {}",
        configured.len(),
        configured
            .iter()
            .map(|a| format!("{} ({})", a.name, a.key))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

fn check_cache(root: &Path) -> Finding {
    let Ok(vol) = crate::vol::identify(root) else {
        return Finding::note("cache not checked (volume unresolved)");
    };
    let dir = crate::config::Config::load()
        .unwrap_or_default()
        .cache_store()
        .join(&vol.uuid);

    if !dir.exists() {
        return Finding::note("no thumbnail cache yet")
            .fix("Built on demand, or all at once with `npc thumbs`.");
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    for e in walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if e.file_type().is_file() {
            files += 1;
            bytes += e.metadata().map_or(0, |m| m.len());
        }
    }
    Finding::fine(format!(
        "thumbnail cache: {files} files, {:.0} MB",
        bytes as f64 / 1e6
    ))
    .fix("`npc thumbs --prune` removes entries nothing asks for any more.")
}

fn check_protection(root: &Path, config: &Config) -> Finding {
    if config.protected_roots.is_empty() {
        return Finding::note("no protected roots configured").fix(
            "A protected root refuses every write, including actions that \
             launch external editors. Worth setting for a drive holding \
             originals.",
        );
    }
    let protected = config.ensure_mutable(root).is_err();
    let list: Vec<String> = config
        .protected_roots
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    Finding::fine(format!(
        "{} protected root(s); this library is {}",
        list.len(),
        if protected { "READ-ONLY" } else { "writable" }
    ))
    .with(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> (tempfile::TempDir, Catalog) {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("750_0001.NEF"), b"raw bytes here").unwrap();
        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();
        (dir, cat)
    }

    fn find<'a>(findings: &'a [Finding], needle: &str) -> &'a Finding {
        findings
            .iter()
            .find(|f| f.title.contains(needle))
            .unwrap_or_else(|| panic!("no finding matching `{needle}` in {findings:#?}"))
    }

    #[test]
    fn a_clean_library_reports_nothing_to_do() {
        let (dir, cat) = library();
        let findings = run(&cat, dir.path(), &Config::default()).unwrap();
        // "no actions available" is a fact about the machine running the
        // test (which editors happen to be on PATH), not about the library
        // -- excluded here so this test asserts library cleanliness only,
        // not incidental host state. A minimal CI container has none of
        // RawTherapee/darktable/GIMP/xdg-open installed and would otherwise
        // fail this exact assertion regardless of the library being clean.
        let relevant: Vec<_> = findings
            .iter()
            .filter(|f| f.title != "no actions available")
            .collect();
        assert!(
            relevant.iter().all(|f| f.severity != Severity::Warn),
            "clean library produced warnings: {:#?}",
            relevant
                .iter()
                .filter(|f| f.severity == Severity::Warn)
                .collect::<Vec<_>>()
        );
    }

    /// The check that exists because this actually happened: an external
    /// program embedded ratings into files on a drive we believed untouched,
    /// and nothing noticed for hours.
    #[test]
    fn an_externally_modified_original_is_reported() {
        let (dir, cat) = library();
        let raw = dir.path().join("2026/08/09/750_0001.NEF");
        std::fs::write(&raw, b"rewritten by something else entirely").unwrap();

        let findings = run(&cat, dir.path(), &Config::default()).unwrap();
        let f = find(&findings, "modified since cataloged");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.detail[0].contains("750_0001.NEF"));
    }

    #[test]
    fn unwritten_edits_are_reported() {
        let (dir, cat) = library();
        let sid = cat
            .stack_id(dir.path().join("2026/08/09").to_str().unwrap(), "750_0001")
            .unwrap()
            .unwrap();
        cat.conn
            .execute("UPDATE stack SET dirty = 1 WHERE id = ?1", [sid])
            .unwrap();

        let findings = run(&cat, dir.path(), &Config::default()).unwrap();
        assert_eq!(find(&findings, "not yet written").severity, Severity::Warn);
    }

    #[test]
    fn a_sidecar_without_an_image_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("2026/08/09");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("750_0001.NEF"), b"raw").unwrap();
        std::fs::write(day.join("ghost.xmp"), b"<x:xmpmeta/>").unwrap();

        let mut cat = Catalog::open_memory().unwrap();
        crate::scan::scan(&mut cat, dir.path(), true).unwrap();

        let findings = run(&cat, dir.path(), &Config::default()).unwrap();
        let f = find(&findings, "no image beside them");
        assert_eq!(f.severity, Severity::Warn);
        assert!(f.detail.iter().any(|d| d.contains("ghost.xmp")));
    }

    #[test]
    fn missing_files_are_a_note_not_a_warning() {
        let (dir, mut cat) = library();
        std::fs::remove_file(dir.path().join("2026/08/09/750_0001.NEF")).unwrap();
        crate::scan::scan(&mut cat, dir.path(), false).unwrap();

        let findings = run(&cat, dir.path(), &Config::default()).unwrap();
        // Deliberately not a warning: tag history outliving a file is the
        // design, not a fault.
        assert_eq!(find(&findings, "not on disk").severity, Severity::Note);
    }

    #[test]
    fn protection_status_is_reported_both_ways() {
        let (dir, cat) = library();

        let open = run(&cat, dir.path(), &Config::default()).unwrap();
        assert_eq!(find(&open, "no protected roots").severity, Severity::Note);

        let guarded = Config {
            protected_roots: vec![dir.path().to_path_buf()],
            ..Default::default()
        };
        let locked = run(&cat, dir.path(), &guarded).unwrap();
        assert!(find(&locked, "protected root").title.contains("READ-ONLY"));
    }
}
