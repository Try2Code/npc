//! Assertions that can only be written against real photographs.
//!
//! Synthetic fixtures would not have caught the phantom-stack bug: it only
//! appears when already-extracted previews sit beside their RAWs, which is
//! what a real Nikon library looks like. Nor would they carry maker notes,
//! preview pyramids, or the orientation tag that decides half the UI.
//!
//! **The set is deliberately minimal**: 11 files, 98 MB, chosen so every
//! property under test is present exactly once. The full 41 GB library is 428x
//! larger and asserts nothing extra. Point `NPC_TEST_LIBRARY` at it to run
//! these against the whole thing.
//!
//! What each file is here for:
//!
//! | file | why |
//! |---|---|
//! | `750_5744.{NEF,JPG,_NEF_embedded.jpg}` | a complete three-member stack |
//! | `750_5744.NEF.pp3` | a profile that must be ignored, not cataloged |
//! | `750_5744_1.NEF` | `_1` is a name, not a derived marker; and a byte-identical twin, so duplicate detection has a real pair |
//! | `750_6278.{NEF,JPG,_NEF_embedded.jpg}` | a second complete stack |
//! | `750_5770_modified.JPG.pp3` | an orphan profile whose image is long gone |
//! | `08/06/750_6338.JPG` | a JPG with no RAW -- the case that makes stack count exceed RAW count |
//! | `08/06/750_6378.JPG` | orientation 1, so both landscape and portrait are covered |
//!
//! Two directories, so events derive into two shooting days.

use npc::db::Catalog;
use npc::scan;
use npc::stack::{classify, Role};
use std::path::{Path, PathBuf};

/// Ground truth, computed in shell over the files rather than from the code:
///
/// ```text
/// 5 distinct stacks; 3 .NEF, 4 .JPG, 2 _NEF_embedded.jpg, 2 .pp3
/// ```
const EXPECTED_STACKS: i64 = 5;
const EXPECTED_RAW: i64 = 3;
const EXPECTED_OOCJPG: i64 = 4;
const EXPECTED_DERIVED: i64 = 2;
const EXPECTED_ORPHANS: i64 = 0;
const EXPECTED_EVENTS: usize = 2;

fn library_path() -> PathBuf {
    std::env::var_os("NPC_TEST_LIBRARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/home/ram/src/tasks/test-data-min"))
}

/// True when running against the set these counts were computed from.
///
/// The exact numbers below are ground truth for one specific directory. Point
/// `NPC_TEST_LIBRARY` elsewhere and they are meaningless -- but the *property*
/// tests (nothing is written, rescanning is idempotent, derived previews fold,
/// events key on capture time) hold for any real library and still run.
fn counts_apply() -> bool {
    std::env::var_os("NPC_TEST_LIBRARY").is_none()
}

macro_rules! require_ground_truth {
    () => {
        if !counts_apply() {
            eprintln!("skipping: exact counts apply only to the bundled minimal set");
            return;
        }
    };
}

fn library() -> Option<PathBuf> {
    let p = library_path();
    p.is_dir().then_some(p)
}

macro_rules! require_library {
    () => {
        match library() {
            Some(p) => p,
            None => {
                eprintln!("skipping: {} not present", library_path().display());
                return;
            }
        }
    };
}

fn scanned() -> Option<Catalog> {
    let root = library()?;
    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, &root, true).unwrap();
    Some(cat)
}

/// The assertion that catches the phantom-stack bug. If derived previews stop
/// folding into their parent, this jumps to 2904 and fails loudly.
#[test]
fn stack_count_matches_ground_truth() {
    require_ground_truth!();
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

/// With profiles out of the picture there is nothing left to orphan here: the
/// three stray .pp3 files are ignored rather than reported. The orphan table
/// still exists for a sidecar whose images have all gone.
#[test]
fn stray_profiles_are_invisible_rather_than_orphans() {
    require_ground_truth!();
    require_library!();
    let cat = scanned().unwrap();
    assert_eq!(cat.orphan_count().unwrap(), EXPECTED_ORPHANS);

    let profiles: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM file WHERE role = 'profile'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(profiles, 0, "a .pp3 was cataloged");
}

/// The two complete stacks each hold a RAW, an out-of-camera JPG and an
/// extracted preview -- and hold them as *one* stack, which is the assertion
/// that catches the phantom-stack bug. If derived previews stop folding, this
/// becomes four stacks of two.
#[test]
fn complete_stacks_hold_all_three_image_roles() {
    require_ground_truth!();
    require_library!();
    let cat = scanned().unwrap();

    let complete: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM (
               SELECT s.id FROM stack s JOIN file f ON f.stack_id = s.id
               WHERE f.role IN ('raw','oocjpg','derived')
               GROUP BY s.id
               HAVING COUNT(DISTINCT f.role) = 3)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(complete, 2, "expected two complete three-role stacks");

    let roles: Vec<String> = cat
        .conn
        .prepare("SELECT DISTINCT role FROM file ORDER BY role")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        roles,
        vec!["derived", "oocjpg", "raw"],
        "with profiles ignored, only the three image roles remain"
    );
}

/// Both orientations are present, because the UI branches on it: a portrait
/// frame puts its metadata panel beside the image, a landscape one below.
#[test]
fn both_orientations_are_represented() {
    require_library!();
    let cat = scanned().unwrap();

    let mut orientations: Vec<i64> = cat
        .conn
        .prepare("SELECT DISTINCT exif_orientation FROM file WHERE exif_orientation IS NOT NULL")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    orientations.sort();
    assert!(
        orientations.contains(&1) && orientations.contains(&8),
        "need a landscape and a portrait frame, got {orientations:?}"
    );
}

/// A byte-identical pair is in the set on purpose, so duplicate detection is
/// exercised against real files rather than only fabricated rows.
#[test]
fn the_duplicate_pair_is_found() {
    require_ground_truth!();
    require_library!();
    let cat = scanned().unwrap();
    let groups = npc::dupes::find(&cat).unwrap();
    assert_eq!(groups.len(), 1, "expected exactly one duplicate group: {groups:?}");
    assert_eq!(groups[0].paths.len(), 2);
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
    // Every image in the set is a real camera file, so every one must date
    // itself. Relative rather than a threshold: the assertion should hold on
    // eleven files or on forty thousand.
    let total = cat.file_count().unwrap();
    assert_eq!(
        with_capture, total,
        "{} of {total} files have no capture time; EXIF parsing regressed",
        total - with_capture
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
        .query_row(
            "SELECT COUNT(DISTINCT lens) FROM file WHERE lens != ''",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(lenses >= 1, "maker-note lens names did not survive");
}

/// Scanning is idempotent: running it twice must not duplicate or drop rows.
#[test]
fn rescanning_changes_nothing() {
    let root = require_library!();
    let mut cat = Catalog::open_memory().unwrap();

    scan::scan(&mut cat, &root, true).unwrap();
    let (s1, f1) = (cat.stack_count().unwrap(), cat.file_count().unwrap());

    let stats = scan::scan(&mut cat, &root, false).unwrap();
    let (s2, f2) = (cat.stack_count().unwrap(), cat.file_count().unwrap());

    assert_eq!((s1, f1), (s2, f2), "a rescan changed the catalog");
    assert_eq!(stats.parsed, 0, "a rescan re-parsed unchanged files");
    assert_eq!(
        stats.unchanged as i64, f1,
        "a rescan should see every file as unchanged"
    );
}

/// Every filename in the library classifies, or is deliberately ignored --
/// nothing silently falls through into a wrong role.
#[test]
fn every_real_filename_classifies_as_expected() {
    require_ground_truth!();
    let root = require_library!();
    let mut counts = std::collections::BTreeMap::new();

    for entry in walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // The whitelist is closed now, so `None` is the expected answer for
        // anything that is not a photograph or our own sidecar.
        if let Some(c) = classify(&name) {
            *counts.entry(c.role.as_str()).or_insert(0usize) += 1;
        }
    }

    assert_eq!(counts.get("raw").copied().unwrap_or(0) as i64, EXPECTED_RAW);
    assert_eq!(
        counts.get("oocjpg").copied().unwrap_or(0) as i64,
        EXPECTED_OOCJPG
    );
    assert_eq!(
        counts.get("derived").copied().unwrap_or(0) as i64,
        EXPECTED_DERIVED
    );
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

    let before = fingerprint_tree(&root);
    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, &root, true).unwrap();
    let after = fingerprint_tree(&root);

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

/// Events are days of shooting, derived from capture time rather than from
/// the directory layout.
#[test]
fn events_are_one_per_day_of_shooting() {
    require_ground_truth!();
    require_library!();
    let cat = scanned().unwrap();

    let days: Vec<(String, i64)> = cat
        .conn
        .prepare(
            "SELECT date(e.start_time,'unixepoch'), COUNT(s.id)
             FROM event e LEFT JOIN stack s ON s.event_id = e.id
             GROUP BY e.id ORDER BY e.start_time",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();

    assert_eq!(days.len(), EXPECTED_EVENTS, "got {days:?}");
    assert_eq!(
        days.iter().map(|(_, n)| n).sum::<i64>(),
        EXPECTED_STACKS,
        "every stack must belong to exactly one event"
    );

    let unassigned: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM stack WHERE event_id IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(unassigned, 0);
}

/// A JPG exported from a NEF carries the RAW's capture time, so it lands in
/// the shoot rather than on the day it was exported. That is the whole reason
/// events key on capture time and not on mtime.
#[test]
fn a_derived_jpg_joins_the_event_of_its_raw() {
    require_library!();
    let cat = scanned().unwrap();

    let mismatched: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM (
               SELECT s.id FROM stack s
               JOIN file f ON f.stack_id = s.id
               WHERE f.capture_time IS NOT NULL
               GROUP BY s.id
               HAVING COUNT(DISTINCT date(f.capture_time,'unixepoch')) > 1)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        mismatched, 0,
        "{mismatched} stacks have members dated to different days"
    );
}

#[test]
fn role_helpers_agree_with_the_library_layout() {
    require_library!();
    assert!(Role::Raw.is_image());
    assert!(!Role::Sidecar.is_image());
}
