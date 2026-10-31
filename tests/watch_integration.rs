//! Real-filesystem, real-inotify tests for `npc::watch` -- unlike the RAW/
//! EXIF-dependent tests in `real_data.rs`, these need no downloaded test
//! photographs (a plain tempdir is enough), so they always run.

use npc::config::{Config, Watch};
use npc::db::Catalog;
use npc::watch::{WatchEvent, Watcher};
use std::fs;
use std::time::Duration;

fn fast_config() -> Config {
    Config {
        watch: Watch {
            enabled: true,
            debounce_ms: 150,
            exclude: Vec::new(),
        },
        ..Config::default()
    }
}

/// Generous on purpose: this asserts something happened within a timeout,
/// not how fast it happened -- matching this project's own "measure, don't
/// tightly bound timing-sensitive things" practice.
const WAIT: Duration = Duration::from_secs(5);

/// Spawn returns before the watches are placed; wait for `Ready` and return
/// how many top-level folders it watches.
fn ready(watcher: &Watcher) -> usize {
    match watcher.changed.recv_timeout(WAIT) {
        Ok(WatchEvent::Ready { dirs }) => dirs,
        other => panic!("expected Ready first, got {other:?}"),
    }
}

/// Wait past any `Scanning` for the next `Rescanned`.
fn next_rescan(watcher: &Watcher) -> std::path::PathBuf {
    loop {
        match watcher.changed.recv_timeout(WAIT) {
            Ok(WatchEvent::Scanning(_)) => {}
            Ok(WatchEvent::Rescanned(dir)) => return dir,
            other => panic!("expected a rescan, got {other:?}"),
        }
    }
}

#[test]
fn a_new_file_outside_the_dated_tree_is_cataloged_without_manual_intervention() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let staging = library.path().join("export/bw");
    fs::create_dir_all(&staging).unwrap();

    let config = fast_config();
    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &config)
        .unwrap()
        .expect("watch.enabled is true");
    assert_eq!(ready(&watcher), 1, "export/");

    fs::write(staging.join("750_1234.JPG"), b"not a real jpeg, just bytes").unwrap();

    assert_eq!(next_rescan(&watcher), staging);

    let (cat, _online) = Catalog::open_for_root_in(store.path(), library.path()).unwrap();
    let id = cat
        .stack_id(&staging.to_string_lossy(), "750_1234")
        .unwrap();
    assert!(id.is_some(), "the new file should have become a stack");
}

/// The specific race `RecursiveMode::Recursive` is trusted to close: a
/// subdirectory created *after* the watcher has already started must still
/// be watched, not silently missed.
#[test]
fn a_subdirectory_created_after_the_watcher_starts_is_still_caught() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    // `export/` exists and is already watched from spawn time; `new-session/`
    // is created only after the watcher is running -- the exact race
    // `RecursiveMode::Recursive` has to close without help, since notify's
    // own catch-up walk (confirmed by reading its inotify backend) silently
    // adds a watch to a newly-discovered directory but never emits a
    // synthetic event for anything already inside it.
    fs::create_dir_all(library.path().join("export")).unwrap();

    let config = fast_config();
    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &config)
        .unwrap()
        .expect("watch.enabled is true");
    ready(&watcher);

    let fresh_dir = library.path().join("export/new-session");
    fs::create_dir_all(&fresh_dir).unwrap();
    fs::write(fresh_dir.join("750_5678.JPG"), b"also not a real jpeg").unwrap();

    // Drain events until the file is actually cataloged or the deadline
    // passes -- a directory-create event for `export/new-session` itself,
    // for its now-stale-looking parent, or both, are all correct outcomes;
    // what matters is that the file ends up in the catalog at all.
    let deadline = std::time::Instant::now() + WAIT;
    let (cat, _online) = Catalog::open_for_root_in(store.path(), library.path()).unwrap();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "timed out waiting for the catalog to catch up"
        );
        match watcher.changed.recv_timeout(remaining) {
            Ok(WatchEvent::Error(e)) => panic!("unexpected watch error: {e}"),
            Ok(WatchEvent::Rescanned(_) | WatchEvent::Scanning(_)) => {}
            Ok(WatchEvent::Ready { .. }) => panic!("Ready twice"),
            Err(e) => panic!("timed out waiting for a rescan: {e}"),
        }
        if cat
            .stack_id(&fresh_dir.to_string_lossy(), "750_5678")
            .unwrap()
            .is_some()
        {
            break;
        }
    }
}

/// The dated `YYYY/MM/DD` tree and `.trash` are npc's own reserved
/// directories -- a file appearing there (a normal `npc import`, or a trash
/// operation moving files) must never trigger a rescan.
#[test]
fn files_in_the_dated_tree_and_trash_are_ignored() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let dated = library.path().join("2026/08/09");
    let trash = library.path().join(".trash/2026/08/09");
    fs::create_dir_all(&dated).unwrap();
    fs::create_dir_all(&trash).unwrap();

    let config = fast_config();
    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &config)
        .unwrap()
        .expect("watch.enabled is true");
    assert_eq!(
        ready(&watcher),
        0,
        "neither the dated tree nor .trash is watched"
    );

    fs::write(dated.join("750_1111.JPG"), b"x").unwrap();
    fs::write(trash.join("750_2222.JPG"), b"x").unwrap();

    // Prove silence, not just absence of a specific event: if either write
    // had armed a rescan, it would have settled and arrived well within
    // this window (debounce is 150ms).
    let outcome = watcher.changed.recv_timeout(Duration::from_millis(800));
    assert!(
        outcome.is_err(),
        "the dated tree and .trash must never trigger a rescan, got {outcome:?}"
    );
}

#[test]
fn watching_is_skipped_entirely_when_disabled_in_config() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let config = Config {
        watch: Watch {
            enabled: false,
            ..Watch::default()
        },
        ..Config::default()
    };

    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &config).unwrap();
    assert!(watcher.is_none());
}

/// Only the root and the top-level folders the watcher acts on are watched
/// -- not the dated tree, whose recursive watch cost npcg ~10 s at startup.
/// A top-level folder that appears later gets its own watch, and files in
/// it are cataloged.
#[test]
fn a_top_level_folder_created_after_the_watcher_starts_is_watched() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    for d in ["2026/08/09", ".trash", "export", "family"] {
        fs::create_dir_all(library.path().join(d)).unwrap();
    }
    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &fast_config())
        .unwrap()
        .expect("watch.enabled is true");
    assert_eq!(ready(&watcher), 2, "export/ and family/ only");

    let fresh = library.path().join("holiday");
    fs::create_dir(&fresh).unwrap();
    // Let the root's create event arrive and the new watch be placed
    // before writing inside -- the very race the rescan of a new folder
    // also covers, so either way the file must end up cataloged.
    std::thread::sleep(Duration::from_millis(300));
    let sub = fresh.join("day1");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("750_4242.JPG"), b"bytes").unwrap();

    let (cat, _online) = Catalog::open_for_root_in(store.path(), library.path()).unwrap();
    let deadline = std::time::Instant::now() + WAIT;
    while cat
        .stack_id(&sub.to_string_lossy(), "750_4242")
        .unwrap()
        .is_none()
    {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            remaining > Duration::ZERO,
            "the file in holiday/day1 was never cataloged"
        );
        let _ = watcher.changed.recv_timeout(remaining);
    }
}

/// Spawning must not wait for the watches: that wait was npcg's ~10 s
/// startup. Even a slow setup returns at once; `Ready` follows.
#[test]
fn spawning_returns_before_the_watches_are_placed() {
    let store = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    let export = library.path().join("export");
    for i in 0..300 {
        fs::create_dir_all(export.join(format!("d{i}/e"))).unwrap();
    }
    let started = std::time::Instant::now();
    let watcher = Watcher::spawn_in(store.path(), library.path().to_path_buf(), &fast_config())
        .unwrap()
        .expect("watch.enabled is true");
    let spawn_took = started.elapsed();
    assert_eq!(ready(&watcher), 1);
    assert!(
        spawn_took < Duration::from_millis(50),
        "spawn blocked for {spawn_took:?}"
    );
}
