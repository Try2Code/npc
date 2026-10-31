//! Watching the library for files that appear outside the dated import tree
//! -- typically a reprocessed export (RawTherapee/GIMP/darktable output)
//! saved back into the library -- and cataloging them in place.
//!
//! Deliberately **not** `import::plan()`/`execute()`: a file that lands
//! inside the library at its final path doesn't need copying, deduping, or
//! a collision suffix -- it needs cataloging, which `scan()` already does,
//! read-only with respect to files. `scan()`'s own "vanished" detection is
//! prefix-scoped to whatever root it's given (see `scan::scan`'s `WHERE dir
//! LIKE ?1`), so rescanning just the directory that changed never touches
//! catalog rows anywhere else in the library.
//!
//! This design went through two rejected framings (a standalone `npc watch
//! <dir>` driving `import`, and hand-rolled `rustix::fs::inotify` before the
//! scope became "the whole library tree, recursively") -- see
//! `PLAN-watch.md` at the repo root for the full history.

use crate::config::Config;
use crate::db::Catalog;
use crate::stack::{classify, Role};
use anyhow::{Context, Result};
use notify::event::{CreateKind, ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// What the watcher thread reports -- drained by the TUI/GUI main loop the
/// same way `thumbs::Cache::done` already is.
#[derive(Debug)]
pub enum WatchEvent {
    /// Watches are in place, on this many top-level folders. Until then
    /// nothing is seen; setting up walks every watched folder.
    Ready {
        dirs: usize,
    },
    /// A settled directory is being rescanned now.
    Scanning(PathBuf),
    Rescanned(PathBuf),
    Error(String),
}

pub struct Watcher {
    pub changed: mpsc::Receiver<WatchEvent>,
    /// Set on drop; the thread checks it at least every 200 ms and exits,
    /// releasing its inotify watches with it.
    stop: Arc<AtomicBool>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Watcher {
    /// Start watching `library_root` for files worth cataloging. Returns at
    /// once: the watches are set up on the watcher's own thread, which says
    /// `WatchEvent::Ready` when done. Set up inline, npcg's window waited
    /// ~10 s for it on a 1,500-folder library on a USB drive.
    ///
    /// Returns `Ok(None)` (not an error) when `config.watch.enabled` is
    /// false, so callers can treat "disabled" and "failed to start"
    /// differently if they ever want to.
    pub fn spawn(library_root: PathBuf, config: &Config) -> Result<Option<Self>> {
        Self::spawn_in(&Catalog::store(), library_root, config)
    }

    /// Split out so tests can point the watcher's own catalog connection at
    /// a temporary store instead of the real XDG one -- same reasoning as
    /// `Catalog::open_for_root`/`open_for_root_in`.
    pub fn spawn_in(store: &Path, library_root: PathBuf, config: &Config) -> Result<Option<Self>> {
        if !config.watch.enabled {
            return Ok(None);
        }

        let (changed_tx, changed_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let debounce = Duration::from_millis(config.watch.debounce_ms);
        let exclude = config.watch.exclude.clone();
        let store = store.to_path_buf();
        let thread_stop = stop.clone();

        std::thread::spawn(move || {
            let started = (|| -> Result<_> {
                let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
                let mut inner = notify::recommended_watcher(move |res| {
                    let _ = raw_tx.send(res);
                })
                .context("creating filesystem watcher")?;
                let watched = add_watches(&mut inner, &library_root, &exclude)?;
                let cat = Catalog::open_for_root_in(&store, &library_root)
                    .context("opening a catalog connection for the watcher")?
                    .0;
                Ok((inner, raw_rx, watched, cat))
            })();
            match started {
                Ok((mut inner, raw_rx, mut watched, cat)) => {
                    let _ = changed_tx.send(WatchEvent::Ready {
                        dirs: watched.len(),
                    });
                    let ctx = Loop {
                        library_root: &library_root,
                        raw_rx: &raw_rx,
                        changed_tx: &changed_tx,
                        debounce,
                        exclude: &exclude,
                        stop: &thread_stop,
                    };
                    run_loop(&ctx, &mut inner, &mut watched, cat);
                }
                Err(e) => {
                    let _ = changed_tx.send(WatchEvent::Error(format!("{e:#}")));
                }
            }
        });

        Ok(Some(Self {
            changed: changed_rx,
            stop,
        }))
    }

    /// A watcher fed by hand, for tests of what consumes its events.
    #[cfg(test)]
    pub(crate) fn from_channel(changed: mpsc::Receiver<WatchEvent>) -> Self {
        Self {
            changed,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Watch the library root itself (not recursively: new files and folders
/// at the top), and each top-level folder the watcher acts on, recursively.
/// Not the dated tree or the trash: their events are ignored anyway, and
/// placing a recursive watch walks and stats every file below it -- 187,262
/// stat calls for the whole library, 92% of them in the dated tree.
fn add_watches(
    inner: &mut RecommendedWatcher,
    library_root: &Path,
    exclude: &[String],
) -> Result<HashSet<PathBuf>> {
    notify::Watcher::watch(inner, library_root, RecursiveMode::NonRecursive)
        .with_context(|| format!("watching {}", library_root.display()))?;
    let mut watched = HashSet::new();
    let entries = std::fs::read_dir(library_root)
        .with_context(|| format!("listing {}", library_root.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        watch_top_level(inner, library_root, &path, exclude, &mut watched);
    }
    Ok(watched)
}

/// Watch `path` recursively if it is a top-level folder of the library the
/// watcher acts on and isn't watched yet. A folder that can't be watched
/// (permissions, gone already) is skipped; the rest still work.
fn watch_top_level(
    inner: &mut RecommendedWatcher,
    library_root: &Path,
    path: &Path,
    exclude: &[String],
    watched: &mut HashSet<PathBuf>,
) {
    let is_dir = std::fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
    if !is_dir
        || path.parent() != Some(library_root)
        || is_excluded(library_root, path, exclude)
        || watched.contains(path)
    {
        return;
    }
    if notify::Watcher::watch(inner, path, RecursiveMode::Recursive).is_ok() {
        watched.insert(path.to_path_buf());
    }
}

/// npc's own reserved top-level directories: the dated import tree
/// (`import::date_path()`'s `YYYY/MM/DD` convention) and the trash. Neither
/// is ever written to by hand -- trash operations in particular move files
/// *into* `.trash`, which would otherwise look exactly like a new import.
/// The desktop's trash on the drive counts too (see `is_trash_dir`).
pub(crate) fn is_reserved_top_level(name: &str) -> bool {
    crate::scan::is_trash_dir(name) || (name.len() == 4 && name.bytes().all(|b| b.is_ascii_digit()))
}

/// True if `path` falls under a reserved or user-configured excluded
/// top-level directory of `library_root`. A path outside `library_root`
/// entirely (shouldn't happen; `notify` only reports what it's watching)
/// is conservatively treated as not excluded.
///
/// Shared with `export::check_target`: the dated import tree and `.trash`
/// are the only parts of the library `npc import`/trash operations ever
/// write to by themselves, so they're the only parts a *different* writer
/// (an export preset, another process) risks colliding with. Everywhere
/// else -- `export/`, or any other subtree -- is exactly what the watcher
/// exists to pick up automatically, so `export` writing there is the
/// intended pipeline, not a hazard to refuse.
pub(crate) fn is_excluded(library_root: &Path, path: &Path, extra: &[String]) -> bool {
    let Ok(rel) = path.strip_prefix(library_root) else {
        return false;
    };
    let Some(top) = rel.components().next() else {
        return false;
    };
    let top = top.as_os_str().to_string_lossy();
    is_reserved_top_level(&top) || extra.iter().any(|e| e == top.as_ref())
}

/// An event worth arming a rescan for. Returns the directory to rescan, or
/// `None` if this event means nothing to npc.
///
/// Two cases:
///
/// - **A new directory appears.** Arms a rescan of the directory *itself*,
///   not its parent -- confirmed necessary, not just defensive, by reading
///   `notify`'s own inotify backend (`inotify.rs`'s `add_watch`): when a new
///   directory is discovered, notify silently walks it to add inotify
///   watches for whatever it already contains, but never emits a synthetic
///   `Create` event for pre-existing files found that way. A file written
///   into a just-created subdirectory before that walk completes would
///   otherwise never be seen at all. Scanning the created directory
///   directly sidesteps the race entirely: `scan()` walks the real
///   filesystem, which recurses into however-many newly-created
///   subdirectories regardless of which inotify watches exist yet -- proven
///   necessary by a real, reproducible integration-test failure without it
///   (`a_subdirectory_created_after_the_watcher_starts_is_still_caught`).
/// - **A file may now be complete**: created, or the temp-file-then-rename-
///   into-place pattern common to RawTherapee/GIMP/darktable. Only for a
///   role `scan()` would actually catalog -- not a `.pp3`/other file
///   `classify()` already ignores, and not our own `.xmp` sidecar (npc's own
///   edits already update the catalog in-process). Arms the file's
///   containing directory.
///
/// Either way, an excluded directory (the dated tree, `.trash`, or a
/// configured extra) never arms anything.
fn qualifying_dir(library_root: &Path, event: &Event, exclude: &[String]) -> Option<PathBuf> {
    // The destination path: the only one present for a plain create or a
    // `RenameMode::To`, and the last of [from, to] for `RenameMode::Both`.
    let path = event.paths.last()?;
    if is_excluded(library_root, path, exclude) {
        return None;
    }
    match event.kind {
        EventKind::Create(CreateKind::Folder) => Some(path.clone()),
        EventKind::Create(CreateKind::File)
        | EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both)) => {
            let name = path.file_name().and_then(|n| n.to_str())?;
            let classified = classify(name)?;
            if classified.role == Role::Sidecar {
                return None;
            }
            path.parent().map(Path::to_path_buf)
        }
        _ => None,
    }
}

/// The debounce loop: waits for either the next raw filesystem event or the
/// nearest pending directory's deadline, whichever comes first, and rescans
/// a directory once it's gone quiet for `debounce`.
/// What `run_loop` works with, bundled so its signature stays readable.
struct Loop<'a> {
    library_root: &'a Path,
    raw_rx: &'a mpsc::Receiver<notify::Result<Event>>,
    changed_tx: &'a mpsc::Sender<WatchEvent>,
    debounce: Duration,
    exclude: &'a [String],
    stop: &'a AtomicBool,
}

fn run_loop(
    ctx: &Loop,
    inner: &mut RecommendedWatcher,
    watched: &mut HashSet<PathBuf>,
    mut cat: Catalog,
) {
    let Loop {
        library_root,
        raw_rx,
        changed_tx,
        debounce,
        exclude,
        stop,
    } = *ctx;
    let mut pending: HashMap<PathBuf, Instant> = HashMap::new();

    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let timeout = pending
            .values()
            .map(|&armed_at| debounce.saturating_sub(armed_at.elapsed()))
            .min()
            .unwrap_or(Duration::from_millis(200))
            .min(Duration::from_millis(200));

        match raw_rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                // A folder new at the top of the library (created, or moved
                // in) gets its own recursive watch; only the root is
                // watched above it.
                // One deleted and recreated is watched afresh.
                for path in &event.paths {
                    if watched.contains(path) && !path.exists() {
                        watched.remove(path);
                    }
                    watch_top_level(inner, library_root, path, exclude, watched);
                }
                if let Some(dir) = qualifying_dir(library_root, &event, exclude) {
                    pending.insert(dir, Instant::now());
                }
            }
            Ok(Err(e)) => {
                if changed_tx.send(WatchEvent::Error(e.to_string())).is_err() {
                    return;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The sender side (the `notify` watcher itself) is gone -- this
            // thread's whole reason to exist just ended.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }

        let ready: Vec<PathBuf> = pending
            .iter()
            .filter(|(_, &armed_at)| armed_at.elapsed() >= debounce)
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in ready {
            pending.remove(&dir);
            if changed_tx.send(WatchEvent::Scanning(dir.clone())).is_err() {
                return;
            }
            let result = crate::scan::scan(&mut cat, &dir, false);
            let event = match result {
                Ok(_) => WatchEvent::Rescanned(dir),
                Err(e) => WatchEvent::Error(e.to_string()),
            };
            if changed_tx.send(event).is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, path: &str) -> Event {
        Event {
            kind,
            paths: vec![PathBuf::from(path)],
            attrs: notify::event::EventAttributes::default(),
        }
    }

    #[test]
    fn is_reserved_top_level_matches_the_dated_tree_and_trash() {
        assert!(is_reserved_top_level("2026"));
        assert!(is_reserved_top_level("1999"));
        assert!(is_reserved_top_level(".trash"));
        assert!(is_reserved_top_level(".Trash-1000"));
        assert!(!is_reserved_top_level("export"));
        assert!(!is_reserved_top_level("20260")); // 5 digits, not a year
        assert!(!is_reserved_top_level("202a")); // not all digits
    }

    #[test]
    fn is_excluded_covers_the_dated_tree_trash_and_config_extras() {
        let root = Path::new("/lib");
        assert!(is_excluded(root, Path::new("/lib/2026/08/09"), &[]));
        assert!(is_excluded(root, Path::new("/lib/.trash/2026/08"), &[]));
        assert!(!is_excluded(root, Path::new("/lib/export/bw"), &[]));
        assert!(is_excluded(
            root,
            Path::new("/lib/inbox/x.jpg"),
            &["inbox".to_string()]
        ));
    }

    /// The exact case this feature exists for: a reprocessed export saved
    /// somewhere outside the dated tree arms a rescan of its own directory.
    #[test]
    fn a_new_jpg_outside_the_dated_tree_arms_its_directory() {
        let root = Path::new("/lib");
        let e = event(
            EventKind::Create(CreateKind::File),
            "/lib/export/bw/750_1234.JPG",
        );
        assert_eq!(
            qualifying_dir(root, &e, &[]),
            Some(PathBuf::from("/lib/export/bw"))
        );
    }

    /// The temp-file-then-rename-into-place pattern common to RawTherapee/
    /// GIMP/darktable batch exports.
    #[test]
    fn a_rename_into_place_also_arms_its_directory() {
        let root = Path::new("/lib");
        let e = event(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            "/lib/export/bw/750_1234.JPG",
        );
        assert_eq!(
            qualifying_dir(root, &e, &[]),
            Some(PathBuf::from("/lib/export/bw"))
        );
    }

    /// A new directory arms *itself*, not its parent -- see `qualifying_dir`'s
    /// doc comment for why this is load-bearing, not just tidy.
    #[test]
    fn a_new_directory_arms_itself_not_its_parent() {
        let root = Path::new("/lib");
        let e = event(
            EventKind::Create(CreateKind::Folder),
            "/lib/export/new-session",
        );
        assert_eq!(
            qualifying_dir(root, &e, &[]),
            Some(PathBuf::from("/lib/export/new-session"))
        );
    }

    #[test]
    fn a_bare_create_of_a_zero_byte_temp_file_is_not_enough_alone() {
        // Only Create(File) and Modify(Name(To|Both)) arm anything -- a
        // plain Modify(Data(..)) mid-write must not trigger a rescan before
        // the file is actually complete.
        let root = Path::new("/lib");
        let e = event(
            EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
            "/lib/export/bw/750_1234.JPG",
        );
        assert_eq!(qualifying_dir(root, &e, &[]), None);
    }

    #[test]
    fn events_inside_the_dated_tree_or_trash_never_arm_anything() {
        let root = Path::new("/lib");
        let dated = event(EventKind::Create(CreateKind::File), "/lib/2026/08/09/x.jpg");
        assert_eq!(qualifying_dir(root, &dated, &[]), None);

        let trashed = event(
            EventKind::Create(CreateKind::File),
            "/lib/.trash/2026/08/09/x.jpg",
        );
        assert_eq!(qualifying_dir(root, &trashed, &[]), None);
    }

    /// npc's own `.xmp` sidecar writes (from `tag`/`rate`/`sync`/the TUI
    /// itself) already update the catalog in the same breath -- reacting to
    /// them would just be redundant churn.
    #[test]
    fn a_sidecar_write_never_arms_a_rescan() {
        let root = Path::new("/lib");
        let e = event(
            EventKind::Create(CreateKind::File),
            "/lib/export/bw/750_1234.xmp",
        );
        assert_eq!(qualifying_dir(root, &e, &[]), None);
    }

    /// A file `classify()` doesn't recognize at all (a `.pp3` profile, a
    /// stray lock file) is not npc's business, whatever directory it's in.
    #[test]
    fn an_unclassified_file_never_arms_a_rescan() {
        let root = Path::new("/lib");
        let e = event(
            EventKind::Create(CreateKind::File),
            "/lib/export/bw/750_1234.NEF.pp3",
        );
        assert_eq!(qualifying_dir(root, &e, &[]), None);
    }
}
