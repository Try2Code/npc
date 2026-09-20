//! Real-filesystem, real-inotify tests for `npc::watch` -- unlike the RAW/
//! EXIF-dependent tests in `real_data.rs`, these need no downloaded test
//! photographs (a plain tempdir is enough), so they always run.

use npc::config::{Config, Watch};
use npc::db::Catalog;
use npc::watch::Watcher;
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

    fs::write(staging.join("750_1234.JPG"), b"not a real jpeg, just bytes").unwrap();

    let event = watcher
        .changed
        .recv_timeout(WAIT)
        .expect("the watcher should have reacted to the new file");
    match event {
        npc::watch::WatchEvent::Rescanned(dir) => assert_eq!(dir, staging),
        npc::watch::WatchEvent::Error(e) => panic!("unexpected watch error: {e}"),
    }

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
            Ok(npc::watch::WatchEvent::Error(e)) => panic!("unexpected watch error: {e}"),
            Ok(npc::watch::WatchEvent::Rescanned(_)) => {}
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
