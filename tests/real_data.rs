//! Assertions that can only be written against the real library.
//!
//! Synthetic fixtures would not have caught the phantom-stack bug: it only
//! shows up when 1182 already-extracted previews sit beside their RAWs, which
//! is what this library actually looks like. The expected numbers here were
//! computed independently of the implementation, by classifying filenames in
//! shell, so this is a comparison against ground truth rather than against
//! whatever the code happens to do.

use npc::db::Catalog;
use npc::scan;
use npc::stack::{classify, Role};
use std::path::Path;

const LIBRARY: &str = "/home/ram/src/tasks/test-data";

// Ground truth, computed by shell over the actual files:
//   1722 distinct .NEF basenames
//   +  68 .JPG with no matching .NEF
//   = 1790 stacks
// and 3 leftover RawTherapee profiles whose images no longer exist.
const EXPECTED_STACKS: i64 = 1790;
const EXPECTED_ORPHANS: i64 = 3;
// Image members only. Deliberately *not* total files: .pp3 profiles and .xmp
// sidecars accumulate as the library is used -- RawTherapee writes a profile
// whenever a shot is opened -- and a test that breaks because the tool was
// used is a broken test, not a broken library.
const EXPECTED_RAW: i64 = 1722;
const EXPECTED_OOCJPG: i64 = 1250;
const EXPECTED_DERIVED: i64 = 1182;
const EXPECTED_STACKS_0809: i64 = 71;

fn library() -> Option<&'static Path> {
    let p = Path::new(LIBRARY);
    p.is_dir().then_some(p)
}

macro_rules! require_library {
    () => {
        match library() {
            Some(p) => p,
            None => {
                eprintln!("skipping: {LIBRARY} not present");
                return;
            }
        }
    };
}

fn scanned() -> Option<Catalog> {
    let root = library()?;
    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, root, true).unwrap();
    Some(cat)
}

/// The assertion that catches the phantom-stack bug. If derived previews stop
/// folding into their parent, this jumps to 2904 and fails loudly.
#[test]
fn stack_count_matches_ground_truth() {
    require_library!();
    let cat = scanned().unwrap();

    assert_eq!(
        cat.stack_count().unwrap(),
        EXPECTED_STACKS,
        "stack count drifted. If this is 2904, derived previews stopped folding \
         into their parent stack and every shot is duplicated."
    );
    for (role, expected) in [
        ("raw", EXPECTED_RAW),
        ("oocjpg", EXPECTED_OOCJPG),
        ("derived", EXPECTED_DERIVED),
    ] {
        let n: i64 = cat
            .conn
            .query_row(
                "SELECT COUNT(*) FROM file WHERE role = ?1 AND present = 1",
                [role],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, expected, "{role} count drifted");
    }
}

/// Leftover profiles whose image is gone must not conjure a stack out of
/// nothing -- they are orphans for `doctor` to report.
#[test]
fn orphan_profiles_do_not_create_stacks() {
    require_library!();
    let cat = scanned().unwrap();

    assert_eq!(cat.orphan_count().unwrap(), EXPECTED_ORPHANS);

    let kinds: Vec<String> = cat
        .conn
        .prepare("SELECT kind FROM orphan")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        kinds.iter().all(|k| k == "profile"),
        "expected only orphan profiles, got {kinds:?}"
    );
}

/// A directory whose composition is known exactly: 71 shots, each with a RAW,
/// an out-of-camera JPG and an extracted preview.
#[test]
fn a_known_directory_resolves_to_three_member_stacks() {
    let root = require_library!();
    let cat = scanned().unwrap();
    let dir = root.join("08/09");

    let stacks: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM stack WHERE dir = ?1",
            [dir.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stacks, EXPECTED_STACKS_0809);

    // Every one of them holds a RAW, an out-of-camera JPG and an extracted
    // preview. Profiles and sidecars may also be present -- those come and go
    // with use -- so the assertion is on the image members, not the total.
    let odd: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM (
               SELECT s.id FROM stack s JOIN file f ON f.stack_id = s.id
               WHERE s.dir = ?1 AND f.role IN ('raw','oocjpg','derived')
               GROUP BY s.id HAVING COUNT(*) != 3)",
            [dir.to_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(odd, 0, "{odd} stacks in 08/09 lack the expected image members");

    // All three image roles are represented. `profile` may or may not appear
    // depending on what has been opened in RawTherapee since.
    let roles: Vec<String> = cat
        .conn
        .prepare(
            "SELECT DISTINCT role FROM file f JOIN stack s ON s.id = f.stack_id
             WHERE s.dir = ?1 ORDER BY role",
        )
        .unwrap()
        .query_map([dir.to_str().unwrap()], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for expected in ["raw", "oocjpg", "derived"] {
        assert!(
            roles.iter().any(|r| r == expected),
            "role `{expected}` missing from 08/09: {roles:?}"
        );
    }
}

/// EXIF came off real files, not fixtures: the D750 body and its maker-note
/// lens names must survive the whole read path into the catalog.
#[test]
fn real_exif_lands_in_the_catalog() {
    require_library!();
    let cat = scanned().unwrap();

    let with_capture: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM file WHERE capture_time IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        with_capture > 4000,
        "only {with_capture} files got a capture time; EXIF parsing regressed"
    );

    let cameras: Vec<String> = cat
        .conn
        .prepare("SELECT DISTINCT camera FROM file WHERE camera != '' ORDER BY camera")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        cameras.iter().any(|c| c.contains("D750")),
        "expected a D750 in {cameras:?}"
    );

    let lenses: i64 = cat
        .conn
        .query_row("SELECT COUNT(DISTINCT lens) FROM file WHERE lens != ''", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(lenses >= 1, "maker-note lens names did not survive");
}

/// Scanning is idempotent: running it twice must not duplicate or drop rows.
#[test]
fn rescanning_changes_nothing() {
    let root = require_library!();
    let mut cat = Catalog::open_memory().unwrap();

    scan::scan(&mut cat, root, true).unwrap();
    let (s1, f1) = (cat.stack_count().unwrap(), cat.file_count().unwrap());

    let stats = scan::scan(&mut cat, root, false).unwrap();
    let (s2, f2) = (cat.stack_count().unwrap(), cat.file_count().unwrap());

    assert_eq!((s1, f1), (s2, f2), "a rescan changed the catalog");
    assert_eq!(stats.parsed, 0, "a rescan re-parsed unchanged files");
    assert!(stats.unchanged > 4000);
}

/// Every filename in the library classifies, or is deliberately ignored --
/// nothing silently falls through into a wrong role.
#[test]
fn every_real_filename_classifies_as_expected() {
    let root = require_library!();
    let mut counts = std::collections::BTreeMap::new();

    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        match classify(&name) {
            Some(c) => *counts.entry(c.role.as_str()).or_insert(0usize) += 1,
            None => panic!("unclassified file in the library: {name}"),
        }
    }

    assert_eq!(counts.get("raw").copied().unwrap_or(0) as i64, EXPECTED_RAW);
    assert_eq!(counts.get("oocjpg").copied().unwrap_or(0) as i64, EXPECTED_OOCJPG);
    assert_eq!(counts.get("derived").copied().unwrap_or(0) as i64, EXPECTED_DERIVED);
    // Profiles and sidecars are not asserted: RawTherapee adds a .pp3 whenever
    // a shot is opened, and this tool adds .xmp whenever anything is tagged.
    // Both are correct behaviour and neither must break the suite.
    // Sidecars are not asserted: test-data is a working sandbox and
    // accumulates them. They are metadata, never counted as photographs --
    // which is exactly what the stack and file counts above verify.
}

/// The library must be untouched by everything above. This is the tripwire in
/// test form: scanning is a read-only operation, and if that ever stops being
/// true the whole design premise is void.
#[test]
fn scanning_never_writes_to_the_library() {
    let root = require_library!();

    let before = fingerprint_tree(root);
    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, root, true).unwrap();
    let after = fingerprint_tree(root);

    assert_eq!(
        before.len(),
        after.len(),
        "the number of files in the library changed during a scan"
    );
    for (path, fp) in &before {
        assert_eq!(
            after.get(path),
            Some(fp),
            "{path} changed during a scan (size or mtime)"
        );
    }
}

fn fingerprint_tree(root: &Path) -> std::collections::BTreeMap<String, (u64, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let md = e.metadata().ok()?;
            Some((
                e.path().to_string_lossy().to_string(),
                (md.size(), md.mtime(), md.mtime_nsec()),
            ))
        })
        .collect()
}

#[test]
fn role_helpers_agree_with_the_library_layout() {
    require_library!();
    assert!(Role::Raw.is_image());
    assert!(!Role::Profile.is_image());
}
