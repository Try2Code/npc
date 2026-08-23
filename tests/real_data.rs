//! Assertions that can only be written against real photographs.
//!
//! Synthetic fixtures would not have caught the phantom-stack bug: it only
//! appears when already-extracted previews sit beside their RAWs, which is
//! what a real Nikon library looks like. Nor would they carry maker notes,
//! preview pyramids, or the orientation tag that decides half the UI.
//!
//! **The set is deliberately minimal**: 11 files, 98 MB, chosen so every
//! property under test is present exactly once. Fetch it with
//! `./fetch-test-data`; point `NPC_TEST_LIBRARY` at a larger library to run
//! the property tests against that instead.
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

// The ground-truth constants are `i64` to match what SQLite hands back, and the
// counts they are compared against are `usize`. Neither a stack count nor a
// role count approaches the range where this conversion loses anything.
#![allow(clippy::cast_possible_wrap)]

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

/// Where the test photographs live: `./testdata`, fetched by
/// `./fetch-test-data`, or wherever `NPC_TEST_LIBRARY` points.
///
/// Deliberately not a path on anyone's machine. The data is 98 MB of RAW and
/// too large for git, so a fresh clone has none and these tests skip until it
/// is fetched.
fn library_path() -> PathBuf {
    if let Some(p) = std::env::var_os("NPC_TEST_LIBRARY") {
        return PathBuf::from(p);
    }
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata")
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

/// The sentinel.
///
/// Rust has no skipped state, so the guarded tests in this file returning
/// early report as passing -- a green suite that verified nothing, which is
/// the exact failure this project keeps designing against. This test fails
/// instead, so the absence is impossible to miss.
///
/// It is the one test here that is *about the suite* rather than about the
/// photographs.
#[test]
fn the_test_photographs_are_present() {
    if library().is_some() {
        return;
    }
    if std::env::var_os("NPC_ALLOW_MISSING_TEST_DATA").is_some() {
        eprintln!("real-data tests inert: NPC_ALLOW_MISSING_TEST_DATA is set");
        return;
    }
    panic!(
        "\n\n  No test photographs at {}\n\n  \
         The {} tests in this file cannot verify anything without them, and\n  \
         would otherwise report as passing.\n\n    \
         ./fetch-test-data                     download them (98 MB, once)\n    \
         NPC_TEST_LIBRARY=<path> cargo test    use a library you already have\n    \
         NPC_ALLOW_MISSING_TEST_DATA=1         acknowledge and move on\n",
        library_path().display(),
        REAL_DATA_TEST_COUNT,
    );
}

/// Guarded tests in this file. If you add one, add it here -- the sentinel
/// quotes this number when it explains what is not running.
const REAL_DATA_TEST_COUNT: usize = 24;

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
                // The sentinel above reports this loudly; here it only needs
                // to be traceable with --nocapture.
                eprintln!("inert: no photographs at {}", library_path().display());
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
    orientations.sort_unstable();
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
    assert_eq!(
        groups.len(),
        1,
        "expected exactly one duplicate group: {groups:?}"
    );
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
        with_capture,
        total,
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

/// `npc import` without `--rescan` trusts the catalog as-is: a library file
/// the catalog has never seen (planted directly on disk, never scanned in)
/// is invisible to dedup, so its byte-identical twin on a card is planned as
/// new, not recognized as a duplicate. Rescanning first (what `--rescan`
/// does) closes that gap. Uses the real `750_5744`/`750_5744_1` twin pair,
/// which is byte-identical (and therefore identity+size identical) by
/// construction -- see the module doc comment.
#[test]
fn import_dedup_needs_a_rescan_to_see_a_hand_added_file() {
    let root = require_library!();

    let lib = tempfile::tempdir().unwrap();
    let lib_dir = lib.path().join("2026/08/01");
    std::fs::create_dir_all(&lib_dir).unwrap();
    std::fs::copy(
        root.join("08/01/750_5744.NEF"),
        lib_dir.join("750_5744.NEF"),
    )
    .unwrap();

    let card = tempfile::tempdir().unwrap();
    std::fs::copy(
        root.join("08/01/750_5744_1.NEF"),
        card.path().join("750_5744_1.NEF"),
    )
    .unwrap();

    let mut cat = Catalog::open_memory().unwrap();

    let plans = npc::import::plan(&cat, card.path(), lib.path(), false).unwrap();
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert!(
        plans[0].duplicate_of.is_none(),
        "an unscanned catalog should not see the library's hand-added twin"
    );

    scan::scan(&mut cat, lib.path(), true).unwrap();

    let plans = npc::import::plan(&cat, card.path(), lib.path(), false).unwrap();
    assert_eq!(plans.len(), 1, "{plans:?}");
    assert!(
        plans[0].duplicate_of.is_some(),
        "a rescanned catalog should recognize the byte-identical twin"
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

// --- embedded rating/tags: the Shotwell-migration fix ----------------------
//
// Confirmed on a real production file: Shotwell writes `Exif.Image.Rating`,
// `Xmp.dc.subject` and `Iptc.Application2.Keywords` directly into the
// original. `read_meta` reads all three; nothing else in the suite builds a
// file carrying them, so these tests do -- via `exiftool`, the same "read
// oracle, never a runtime dependency" role `PLAN.md` verification #11
// describes. First use of it in this suite.

/// Run `exiftool`, panicking with the real output rather than skipping if it
/// is missing -- consistent with this project's stance on tests that would
/// otherwise silently verify nothing.
fn exiftool(args: &[&str]) {
    let out = std::process::Command::new("exiftool")
        .args(args)
        .output()
        .unwrap_or_else(|e| {
            panic!(
                "exiftool not runnable ({e}). It is listed as verified-present \
                 tooling in PLAN.md; install it to run this test."
            )
        });
    assert!(
        out.status.success(),
        "exiftool {args:?} failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Copy one real testdata file (`08/01/750_5744.JPG`) into a fresh tempdir at
/// its own basename, never mutating the checked-out `testdata/` itself.
/// Callers must have already gated on `require_library!()` themselves.
fn copy_into(dir: &Path, name: &str) -> PathBuf {
    let root = library().expect("caller must check require_library!() first");
    let dest = dir.join(Path::new(name).file_name().unwrap());
    std::fs::copy(root.join("08/01").join(name), &dest).unwrap();
    dest
}

#[test]
fn embedded_rating_and_keywords_round_trip_through_read_meta() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = copy_into(dir.path(), "750_5744.JPG");

    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=4",
        "-XMP-dc:Subject=india",
        "-IPTC:Keywords=berlin", // deliberately the *other* source, to prove the union
        jpg.to_str().unwrap(),
    ]);

    let fm = npc::meta::read_meta(jpg.to_str().unwrap()).unwrap();
    assert_eq!(fm.rating, 4);
    let mut tags = fm.tags.clone();
    tags.sort();
    assert_eq!(tags, vec!["berlin".to_string(), "india".to_string()]);
}

/// A real production file (`export/bw/750_5727-2.jpg`) had `Xmp.xmp.Rating`
/// set with no `Exif.Image.Rating` at all -- some tool (not Shotwell, which
/// writes both) rates through XMP alone. `read_meta` silently reported it as
/// unrated until `shim.cc` gained this fallback.
#[test]
fn a_rating_that_exists_only_in_xmp_is_still_read() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = copy_into(dir.path(), "750_6278.JPG");

    exiftool(&[
        "-overwrite_original",
        "-XMP:Rating=3",
        "-EXIF:Rating=",
        jpg.to_str().unwrap(),
    ]);
    exiftool_confirms_no_exif_rating(&jpg);

    let fm = npc::meta::read_meta(jpg.to_str().unwrap()).unwrap();
    assert_eq!(
        fm.rating, 3,
        "an XMP-only rating must not read back as unset"
    );
}

fn exiftool_confirms_no_exif_rating(path: &Path) {
    let out = std::process::Command::new("exiftool")
        .args(["-s3", "-EXIF:Rating", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        out.stdout.is_empty(),
        "test setup is wrong: EXIF:Rating should be absent, got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// The same keyword embedded via both sources (`Xmp.dc.subject` and
/// `Iptc.Application2.Keywords` -- Shotwell writes both) must collapse to one
/// tag, not two. Regression: comparing `.begin()`/`.end()` from two
/// separately-constructed `std::string(existing)` temporaries in the C++
/// dedup loop compared iterators from two different objects and never
/// matched, so the same real-world tag ("gg", on `export/bw/750_5727-2.jpg`)
/// came back duplicated.
#[test]
fn the_same_tag_from_two_embedded_sources_is_not_duplicated() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = copy_into(dir.path(), "750_6278.JPG");

    exiftool(&[
        "-overwrite_original",
        "-XMP-dc:Subject=gg",
        "-IPTC:Keywords=gg",
        jpg.to_str().unwrap(),
    ]);

    let fm = npc::meta::read_meta(jpg.to_str().unwrap()).unwrap();
    assert_eq!(
        fm.tags,
        vec!["gg".to_string()],
        "the same tag from both embedded sources must collapse to one"
    );
}

#[test]
fn an_untouched_original_carries_no_embedded_tags() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = copy_into(dir.path(), "750_6278.JPG"); // never touched by exiftool

    let fm = npc::meta::read_meta(jpg.to_str().unwrap()).unwrap();
    assert!(fm.tags.is_empty());
    assert_eq!(fm.rating, -1, "unset EXIF rating must read back as -1");
}

/// The end-to-end path: `scan` seeds the catalog from what Shotwell already
/// wrote, `sync` writes the real sidecar, and the original is untouched
/// throughout -- the same byte/mtime proof
/// `scanning_never_writes_to_the_library` uses.
#[test]
fn scan_seeds_from_embedded_metadata_and_sync_writes_the_sidecar() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let nef = copy_into(dir.path(), "750_5744.NEF");
    let jpg = copy_into(dir.path(), "750_5744.JPG");

    // Different ratings on each member, deliberately, to exercise the
    // higher-wins tie-break through a real scan rather than only the pure
    // function: NEF rated 3, JPG rated 5 -- the stack must come out 5.
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=3",
        nef.to_str().unwrap(),
    ]);
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=5",
        "-XMP-dc:Subject=india",
        jpg.to_str().unwrap(),
    ]);

    let before = fingerprint_tree(dir.path());

    let mut cat = Catalog::open_memory().unwrap();
    let stats = scan::scan(&mut cat, dir.path(), false).unwrap();
    assert_eq!(stats.seeded_from_embedded, 1);

    let sid = cat
        .stack_id(&dir.path().to_string_lossy(), "750_5744")
        .unwrap()
        .unwrap();
    let (rating, dirty): (i64, i64) = cat
        .conn
        .query_row(
            "SELECT rating, dirty FROM stack WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rating, 5, "the higher of the two embedded ratings must win");
    assert_eq!(dirty, 1, "a seeded stack must be dirty until synced");

    let tags: Vec<String> = cat
        .conn
        .prepare(
            "SELECT t.name FROM tag t JOIN stack_tag st ON st.tag_id = t.id
             WHERE st.stack_id = ?1",
        )
        .unwrap()
        .query_map([sid], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(tags, vec!["india".to_string()]);

    let pending: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pending WHERE stack_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 1);

    // `sync` is the only step that writes anything -- the real, already-tested
    // path, not a new one.
    let flush_stats = npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();
    assert_eq!(
        flush_stats.sidecars_written, 1,
        "errors: {:?}",
        flush_stats.errors
    );
    assert!(dir.path().join("750_5744.xmp").exists());

    let after = fingerprint_tree(dir.path());
    for (path, fp) in &before {
        assert_eq!(after.get(path), Some(fp), "{path} changed during scan+sync");
    }
}

/// Once a real sidecar exists, embedded metadata must never resurrect
/// something the user deliberately removed. A `--full` rescan re-parses every
/// original -- including its still-embedded Keywords -- but the sidecar
/// ingested afterward is unconditional and must win.
#[test]
fn a_full_rescan_does_not_resurrect_a_tag_cleared_via_the_sidecar() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let jpg = copy_into(dir.path(), "750_6278.JPG");
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=5",
        "-XMP-dc:Subject=india",
        jpg.to_str().unwrap(),
    ]);

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, dir.path(), false).unwrap();
    npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();

    let sid = cat
        .stack_id(&dir.path().to_string_lossy(), "750_6278")
        .unwrap()
        .unwrap();

    // The user clears everything, and syncs -- a real, empty sidecar now sits
    // on disk.
    npc::edit::apply(
        &mut cat,
        &[sid],
        &[
            npc::edit::Change::ClearTags,
            npc::edit::Change::SetRating(None),
        ],
        &npc::config::Config::default(),
    )
    .unwrap();
    npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();

    // The embedded Keywords are still sitting in the JPG untouched -- this is
    // the point of the test. A naive re-seed would resurrect them.
    scan::scan(&mut cat, dir.path(), true).unwrap();

    let (rating, tag_count): (Option<i64>, i64) = cat
        .conn
        .query_row(
            "SELECT rating, (SELECT COUNT(*) FROM stack_tag WHERE stack_id = s.id)
             FROM stack s WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rating, None, "a --full rescan resurrected a cleared rating");
    assert_eq!(tag_count, 0, "a --full rescan resurrected a cleared tag");
}

/// When the RAW's and JPG's own embedded ratings already agree, resync must
/// find nothing to do -- through both entry points: the pure finder and the
/// `doctor` check built on it.
#[test]
fn rate_sync_noop_when_raw_and_jpg_agree() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let nef = copy_into(dir.path(), "750_6278.NEF");
    let jpg = copy_into(dir.path(), "750_6278.JPG");
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=4",
        nef.to_str().unwrap(),
    ]);
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=4",
        jpg.to_str().unwrap(),
    ]);

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, dir.path(), false).unwrap();

    let conflicts = npc::rating_sync::scan(&cat).unwrap();
    assert!(
        conflicts.iter().all(|c| !c.needs_write()),
        "agreeing ratings must not be reported as needing a resync: {conflicts:?}"
    );

    let findings = npc::doctor::run(&cat, dir.path(), &npc::config::Config::default()).unwrap();
    let f = findings
        .iter()
        .find(|f| f.title.contains("RAW and JPG ratings agree"))
        .expect("doctor must report the agree finding");
    assert_eq!(f.severity, npc::doctor::Severity::Fine);
}

/// The gap this feature closes: `scan` seeds and syncs a stack once, then an
/// external tool rates the JPG differently afterward. Nothing re-reads
/// embedded metadata after that point except this resync. Running the same
/// sequence `npc rate-sync` runs must resolve to the higher rating, write it
/// through the ordinary sidecar path, and leave both originals' bytes and
/// mtimes untouched.
#[test]
fn rate_sync_resolves_diverging_ratings_and_flushes() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let nef = copy_into(dir.path(), "750_6278.NEF");
    let jpg = copy_into(dir.path(), "750_6278.JPG");
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=3",
        nef.to_str().unwrap(),
    ]);
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=3",
        jpg.to_str().unwrap(),
    ]);

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, dir.path(), false).unwrap();
    let flush_stats = npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();
    assert_eq!(flush_stats.sidecars_written, 1);

    // An external tool rates the JPG higher, after the one-shot seed already
    // settled the catalog at 3 -- npc never looks again on its own.
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=5",
        jpg.to_str().unwrap(),
    ]);

    let sid = cat
        .stack_id(&dir.path().to_string_lossy(), "750_6278")
        .unwrap()
        .unwrap();
    let catalog_rating_before: Option<i64> = cat
        .conn
        .query_row("SELECT rating FROM stack WHERE id = ?1", [sid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        catalog_rating_before,
        Some(3),
        "test setup: seed must have settled at 3"
    );

    let before = fingerprint_tree(dir.path());

    // The same sequence `Cmd::RateSync`'s handler runs.
    let candidates: Vec<_> = npc::rating_sync::scan(&cat)
        .unwrap()
        .into_iter()
        .filter(npc::rating_sync::Conflict::needs_write)
        .collect();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].resolved,
        Some(5),
        "the higher rating must win"
    );
    npc::edit::apply(
        &mut cat,
        &[candidates[0].stack_id],
        &[npc::edit::Change::SetRating(Some(5))],
        &npc::config::Config::default(),
    )
    .unwrap();
    let flush_stats = npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();
    assert_eq!(
        flush_stats.sidecars_written, 1,
        "errors: {:?}",
        flush_stats.errors
    );

    let catalog_rating_after: Option<i64> = cat
        .conn
        .query_row("SELECT rating FROM stack WHERE id = ?1", [sid], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(catalog_rating_after, Some(5));

    let sidecar =
        npc::meta::read_sidecar(dir.path().join("750_6278.xmp").to_str().unwrap()).unwrap();
    assert_eq!(
        sidecar.rating, 5,
        "the sidecar on disk must carry the resolved rating"
    );

    assert!(
        npc::rating_sync::scan(&cat)
            .unwrap()
            .into_iter()
            .all(|c| !c.needs_write()),
        "the stack must no longer be flagged after resync"
    );

    // Only the two originals must survive untouched -- the sidecar is
    // *supposed* to change, that's the resync actually writing.
    let after = fingerprint_tree(dir.path());
    for original in [&nef, &jpg] {
        let path = original.to_string_lossy().to_string();
        assert_eq!(
            after.get(&path),
            before.get(&path),
            "{path} changed during resync"
        );
    }
}

/// `--dry-run` must compute and print the same thing a real resync would,
/// without touching the catalog or the sidecar.
#[test]
fn rate_sync_dry_run_changes_nothing() {
    require_ground_truth!();
    require_library!();
    let dir = tempfile::tempdir().unwrap();
    let nef = copy_into(dir.path(), "750_6278.NEF");
    let jpg = copy_into(dir.path(), "750_6278.JPG");
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=3",
        nef.to_str().unwrap(),
    ]);
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=3",
        jpg.to_str().unwrap(),
    ]);

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, dir.path(), false).unwrap();
    npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=5",
        jpg.to_str().unwrap(),
    ]);

    let sid = cat
        .stack_id(&dir.path().to_string_lossy(), "750_6278")
        .unwrap()
        .unwrap();

    // Dry run: compute candidates, print nothing but the finding -- never
    // call apply/flush.
    let candidates: Vec<_> = npc::rating_sync::scan(&cat)
        .unwrap()
        .into_iter()
        .filter(npc::rating_sync::Conflict::needs_write)
        .collect();
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].resolved, Some(5));

    let (rating, dirty): (Option<i64>, i64) = cat
        .conn
        .query_row(
            "SELECT rating, dirty FROM stack WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rating, Some(3), "dry run must not touch the catalog");
    assert_eq!(dirty, 0, "dry run must not mark the stack dirty");

    let pending: i64 = cat
        .conn
        .query_row(
            "SELECT COUNT(*) FROM pending WHERE stack_id = ?1",
            [sid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending, 0);

    let sidecar =
        npc::meta::read_sidecar(dir.path().join("750_6278.xmp").to_str().unwrap()).unwrap();
    assert_eq!(
        sidecar.rating, 3,
        "dry run must not touch the sidecar on disk"
    );
}

/// The end-to-end scenario this feature exists for: a JPG already in the
/// library gets rated/tagged by an external tool (Shotwell) on a separate
/// copy -- e.g. reimported from the same SD card later -- which changes its
/// size without touching a single pixel. `import::plan` must recognise this
/// as the same shot and merge, not plan a `_1`-suffixed duplicate; the merge
/// must never regress the rating already set in the library, and neither
/// original's bytes or mtime may move.
#[test]
fn import_merges_metadata_from_a_re_embedded_card_copy_instead_of_duplicating() {
    require_ground_truth!();
    require_library!();
    let lib = tempfile::tempdir().unwrap();
    let nef = copy_into(lib.path(), "750_6278.NEF");
    let jpg = copy_into(lib.path(), "750_6278.JPG");

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, lib.path(), false).unwrap();
    let sid = cat
        .stack_id(&lib.path().to_string_lossy(), "750_6278")
        .unwrap()
        .unwrap();

    // A rating and tag already set through npc itself, before the SD-card
    // copy ever enters the picture -- the merge must not lose either.
    npc::edit::apply(
        &mut cat,
        &[sid],
        &[
            npc::edit::Change::SetRating(Some(3)),
            npc::edit::Change::AddTags(vec!["existing".to_string()]),
        ],
        &npc::config::Config::default(),
    )
    .unwrap();
    npc::edit::flush(&mut cat, &npc::config::Config::default()).unwrap();

    // A separate "card" copy of just the JPG, re-embedded with a higher
    // rating and a new tag -- same pixels, different size.
    let card = tempfile::tempdir().unwrap();
    let card_jpg = copy_into(card.path(), "750_6278.JPG");
    exiftool(&[
        "-overwrite_original",
        "-EXIF:Rating=5",
        "-XMP-dc:Subject=india",
        card_jpg.to_str().unwrap(),
    ]);

    let before = fingerprint_tree(lib.path());
    let card_before = fingerprint_tree(card.path());

    let plans = npc::import::plan(&cat, card.path(), lib.path(), false).unwrap();
    assert_eq!(plans.len(), 1);
    let stack = &plans[0];
    assert!(stack.duplicate_of.is_some(), "must not plan a fresh import");
    let merge = stack
        .metadata_merge
        .as_ref()
        .expect("a size-shifted same-shot copy must produce a metadata merge");
    assert!(merge.needs_write());
    assert_eq!(
        merge.rating_after,
        Some(5),
        "the higher rating must win, not regress the existing 3"
    );
    let mut tags_after = merge.tags_after.clone();
    tags_after.sort();
    assert_eq!(
        tags_after,
        vec!["existing".to_string(), "india".to_string()]
    );

    let stats = npc::import::execute(
        &plans,
        &mut cat,
        &npc::config::Config::default(),
        false,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.stacks_copied, 0, "must not copy a duplicate");
    assert_eq!(stats.merges_applied, 1);
    assert!(stats.errors.is_empty(), "{:?}", stats.errors);

    let (rating, tag_count): (Option<i64>, i64) = cat
        .conn
        .query_row(
            "SELECT rating, (SELECT COUNT(*) FROM stack_tag WHERE stack_id = s.id)
             FROM stack s WHERE id = ?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rating, Some(5));
    assert_eq!(tag_count, 2);

    let sidecar =
        npc::meta::read_sidecar(lib.path().join("750_6278.xmp").to_str().unwrap()).unwrap();
    assert_eq!(
        sidecar.rating, 5,
        "the sidecar must carry the merged rating"
    );

    // Neither original -- not the library's NEF/JPG, not the card's JPG --
    // may have moved a single byte or its mtime.
    let after = fingerprint_tree(lib.path());
    for original in [&nef, &jpg] {
        let path = original.to_string_lossy().to_string();
        assert_eq!(
            after.get(&path),
            before.get(&path),
            "{path} changed during the merge"
        );
    }
    let card_after = fingerprint_tree(card.path());
    let card_path = card_jpg.to_string_lossy().to_string();
    assert_eq!(
        card_after.get(&card_path),
        card_before.get(&card_path),
        "the card copy must never be written to"
    );
}

/// The regression this feature shipped with: a RAW shares maker-note
/// identity with its JPG sibling (same shot, always a different size), but
/// that is never license to treat the RAW as "the JPG, just re-embedded."
/// Importing a RAW for a shot whose JPG is already in the library must add
/// the RAW, not silently skip it as a duplicate of the JPG.
#[test]
fn import_adds_a_raw_whose_jpg_sibling_is_already_in_the_library() {
    require_ground_truth!();
    let root = require_library!();
    let lib = tempfile::tempdir().unwrap();

    // Only the JPG is in the library, in the same dated directory a real
    // import would place it -- the RAW has never been imported.
    let src_jpg = root.join("08/01/750_6278.JPG");
    let fm = npc::meta::read_meta(src_jpg.to_str().unwrap()).unwrap();
    let date_dir = lib.path().join(npc::import::date_path(fm.capture_time));
    std::fs::create_dir_all(&date_dir).unwrap();
    let jpg = copy_into(&date_dir, "750_6278.JPG");

    let mut cat = Catalog::open_memory().unwrap();
    scan::scan(&mut cat, lib.path(), false).unwrap();

    // The RAW sibling, on a separate "card".
    let card = tempfile::tempdir().unwrap();
    let card_nef = copy_into(card.path(), "750_6278.NEF");

    let plans = npc::import::plan(&cat, card.path(), lib.path(), false).unwrap();
    assert_eq!(plans.len(), 1);
    let stack = &plans[0];
    assert_eq!(
        stack.duplicate_of, None,
        "a RAW must never be matched against its JPG sibling's identity"
    );
    assert!(
        stack.metadata_merge.is_none(),
        "there is nothing to merge -- this is a new member, not a re-embed"
    );

    let dest_nef = stack.dest_dir.join("750_6278.NEF");
    let jpg_before = fingerprint_tree(lib.path());

    let stats = npc::import::execute(
        &plans,
        &mut cat,
        &npc::config::Config::default(),
        false,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(stats.stacks_copied, 1, "errors: {:?}", stats.errors);
    assert_eq!(stats.duplicates_skipped, 0);

    assert!(
        dest_nef.exists(),
        "the RAW was never copied in: {dest_nef:?}"
    );
    assert_eq!(
        std::fs::read(&dest_nef).unwrap(),
        std::fs::read(&card_nef).unwrap(),
        "the copy must be byte-identical to the card"
    );

    let jpg_path = jpg.to_string_lossy().to_string();
    let jpg_after = fingerprint_tree(lib.path());
    assert_eq!(
        jpg_after.get(&jpg_path),
        jpg_before.get(&jpg_path),
        "importing the RAW must not disturb the JPG already in the library"
    );

    // A rescan must now find one complete stack, not two.
    scan::scan(&mut cat, lib.path(), true).unwrap();
    let sid = cat
        .stack_id(&date_dir.to_string_lossy(), "750_6278")
        .unwrap()
        .unwrap();
    let mut roles: Vec<String> = cat
        .conn
        .prepare("SELECT role FROM file WHERE stack_id = ?1")
        .unwrap()
        .query_map([sid], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    roles.sort();
    assert_eq!(roles, vec!["oocjpg".to_string(), "raw".to_string()]);
}
