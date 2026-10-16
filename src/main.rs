use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use npc::{db::Catalog, meta, scan, vol};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "npc", about = "Terminal photo catalog", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// The library root, accepted either positionally or as `--root`.
///
/// `scan`, `status` and `tui` took a bare positional while every other
/// subcommand took `--root`, so `npc scan --root <path>` failed with an
/// unknown-flag error -- including when that is exactly what the tool had just
/// advised in `library()` below. Accepting both keeps the short positional form
/// and makes the flag uniform across the CLI.
#[derive(clap::Args)]
struct RootArg {
    /// Library root
    #[arg(value_name = "ROOT")]
    positional: Option<PathBuf>,
    /// Library root, for symmetry with the other subcommands
    #[arg(long = "root", value_name = "PATH", conflicts_with = "positional")]
    flag: Option<PathBuf>,
}

impl RootArg {
    fn get(&self) -> Option<PathBuf> {
        self.flag.clone().or_else(|| self.positional.clone())
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Read everything we can learn about one file. Proves the Exiv2 bridge.
    Probe {
        path: String,
        /// Also report which embedded preview would serve this long-edge size
        #[arg(long)]
        preview: Option<u32>,
    },
    /// Walk a library into the catalog. Read-only with respect to your files.
    Scan {
        #[command(flatten)]
        root: RootArg,
        /// Reparse every file rather than trusting size+mtime
        #[arg(long)]
        full: bool,
        /// Limit the walk to this directory instead of the whole library --
        /// repeatable. For a late addition dropped somewhere specific (the
        /// watcher's own job while npc tui/npcg is open, but there is no
        /// daemon for when neither is running): a full walk of a
        /// tens-of-thousands-of-stacks library to catalog a handful of new
        /// files is the exact cost this exists to skip. Must be inside the
        /// library; "vanished" detection stays scoped to each one given,
        /// same as a directory the watcher rescans on its own.
        #[arg(long, value_name = "PATH")]
        only: Vec<PathBuf>,
    },
    /// Report which volume a path lives on, and where its catalog would go.
    Volume { path: PathBuf },
    /// Catalog summary.
    Status {
        #[command(flatten)]
        root: RootArg,
    },
    /// Select stacks with the query language. See `npc find --help-syntax`.
    Find {
        /// e.g. "beach and rating>=4", "year=2026 camera~D750 -private"
        #[arg(default_value = "")]
        query: String,
        /// Which library to search
        #[arg(long)]
        root: Option<PathBuf>,
        /// Print every file of each matching stack rather than the stack
        #[arg(long)]
        files: bool,
        /// NUL-separated output, for `xargs -0`
        #[arg(long)]
        print0: bool,
        #[arg(long)]
        limit: Option<usize>,
        /// Print the compiled SQL instead of running it
        #[arg(long)]
        explain: bool,
        /// Print the query language reference and exit
        #[arg(long)]
        help_syntax: bool,
    },
    /// List tags with counts.
    Tags {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Add or remove tags on the stacks a query selects.
    Tag {
        /// `add` or `rm`
        action: String,
        /// Comma-separated tag names
        names: String,
        #[command(flatten)]
        target: Target,
    },
    /// Set a star rating (0-5, or `-` to clear).
    Rate {
        value: String,
        #[command(flatten)]
        target: Target,
    },
    /// Re-resolve a stack's rating and tags from its RAW's and JPG's own
    /// embedded metadata, read live off the files -- not the catalog's
    /// merged value. The higher rating wins and tags are unioned, the same
    /// rules `scan` applies once when a stack is first seen. `npc doctor`
    /// reports stacks whose ratings disagree, or whose embedded metadata is
    /// present on only one side.
    RateSync {
        #[command(flatten)]
        target: Target,
    },
    /// Mark stacks flagged (pick) or rejected. These are mutually exclusive.
    Mark {
        /// `flag`, `reject`, or `clear`
        state: String,
        #[command(flatten)]
        target: Target,
    },
    /// Rotate. Writes orientation to the sidecar; no pixels are touched and
    /// the original is not opened for writing.
    Rotate {
        /// `cw`, `ccw`, `180`, or `none` to clear
        direction: String,
        #[command(flatten)]
        target: Target,
    },
    /// Run a configured action on the stacks a query selects.
    Exec {
        /// Action name or key, as listed by `npc exec --list`
        #[arg(default_value = "")]
        action: String,
        #[arg(short = 'q', long)]
        query: Option<String>,
        #[arg(long)]
        root: Option<PathBuf>,
        /// Show the available actions and exit
        #[arg(long)]
        list: bool,
        /// Print the command instead of running it
        #[arg(long)]
        dry_run: bool,
    },
    /// Set a title or description.
    Set {
        /// `title` or `description`
        field: String,
        value: String,
        #[command(flatten)]
        target: Target,
    },
    /// Generate thumbnails ahead of time.
    Thumbs {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Restrict to the stacks a query selects
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// `grid` or `view`; sizes come from config.toml
        #[arg(long, default_value = "grid")]
        size: String,
        /// Delete cache entries nothing would ask for any more
        #[arg(long)]
        prune: bool,
        /// Internal: a file of newline-separated stack ids, as an
        /// alternative to `--query`. Not documented as a user-facing
        /// selector -- this exists so `import` can hand off an exact id
        /// list to a detached background prewarm process without going
        /// through the query language.
        #[arg(long, hide = true, conflicts_with = "query")]
        ids_file: Option<PathBuf>,
    },
    /// Report what this terminal can actually do. Run it inside the terminal
    /// you use for `npc tui`.
    Term,
    /// Write resized copies into a directory outside the library.
    Export {
        /// Which stacks
        #[arg(short = 'q', long)]
        query: Option<String>,
        /// Where the copies go. Must be outside the library. Falls back to
        /// `export_dir` in config.toml when not given.
        #[arg(short = 'o', long)]
        out: Option<PathBuf>,
        #[arg(long)]
        root: Option<PathBuf>,
        /// Long edge in pixels
        #[arg(long, default_value_t = npc::export::DEFAULT_LONG_EDGE)]
        size: u32,
        /// JPEG quality, 1-100
        #[arg(long, default_value_t = npc::export::DEFAULT_QUALITY)]
        quality: u8,
        /// Use a named preset from config.toml instead of the built-in resize
        #[arg(long)]
        preset: Option<String>,
        /// Mirror YYYY/MM/DD under the target
        #[arg(long)]
        keep_structure: bool,
        /// Do not carry tags and rating onto the copies
        #[arg(long)]
        strip: bool,
        #[arg(long)]
        dry_run: bool,
        /// List the presets defined in config.toml and exit
        #[arg(long)]
        list_presets: bool,
    },
    /// Emit a shell completion script.
    Completion {
        /// bash, zsh, fish, elvish or powershell
        shell: clap_complete::Shell,
    },
    /// The browser.
    Tui {
        #[command(flatten)]
        root: RootArg,
        /// Show trashed stacks alongside the rest of the library
        #[arg(long, conflicts_with = "trash")]
        all: bool,
        /// Show only the trash, nothing else
        #[arg(long)]
        trash: bool,
        /// Disable the background library watcher for this session
        /// (overrides `[watch] enabled` in config.toml)
        #[arg(long)]
        no_watch: bool,
    },
    /// Write every pending edit out to its sidecar.
    Sync {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Copy photos from a card into a dated library. The source is never modified.
    Import {
        /// Where the photos are now (a card, an inbox directory). If
        /// omitted, mounted cards are detected and you're prompted to pick.
        source: Option<PathBuf>,
        /// Library root; files land in <library>/YYYY/MM/DD/. Defaults to
        /// the configured library, like every other subcommand.
        #[arg(long)]
        library: Option<PathBuf>,
        /// Show the plan without copying anything
        #[arg(long)]
        dry_run: bool,
        /// Hash-verify an identity match before trusting it as a duplicate,
        /// not just its size. Slower -- reads both files fully -- but catches
        /// a same-size, byte-preserving edit that a size check alone would
        /// miss. Off by default: measured too slow to hash every
        /// already-imported stack on every re-scan.
        #[arg(long)]
        verify_content: bool,
        /// Rescan the whole library for freshness before planning, like every
        /// import used to. Off by default: trusts the existing catalog, which
        /// is much faster but can miss a duplicate if something outside npc
        /// touched the library since the last scan (a file dropped in by
        /// hand, or another tool's output).
        #[arg(long)]
        rescan: bool,
        /// Confirm every card file by its shutter count, skipping the cheap
        /// first check. By default a card file whose name and size -- and,
        /// for files imported from now on, card mtime -- match a library file
        /// counts as imported without being read.
        #[arg(long)]
        deep: bool,
    },
    /// List shooting days, and give them names.
    Events {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Name an event: `npc events --name 3 "Ostsee"`. Names survive
        /// rescanning; pass an empty string to clear one.
        #[arg(long, num_args = 2, value_names = ["ID", "NAME"])]
        name: Option<Vec<String>>,
    },
    /// Check the catalog against what is actually on disk.
    Doctor {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Exit non-zero if anything needs attention, for scripting
        #[arg(long)]
        strict: bool,
    },
    /// Report shots that appear more than once.
    Dupes {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Move stacks to <library>/.trash/. Reversible; nothing is deleted.
    Trash {
        #[command(flatten)]
        target: Target,
    },
    /// Put a trashed stack back where it came from.
    Untrash {
        /// Stack basename, as shown by `trash-list`
        base: String,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// What is currently in the trash.
    TrashList {
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Permanently delete everything in the trash. The only real delete.
    TrashEmpty {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Required. Without it this only reports what would go.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(clap::Args, Clone)]
struct Target {
    /// Which stacks to act on, in the query language
    #[arg(short = 'q', long)]
    query: Option<String>,
    /// Explicit stack paths (dir/base), if not using a query
    paths: Vec<String>,
    #[arg(long)]
    root: Option<PathBuf>,
    /// Show what would change without changing it
    #[arg(long)]
    dry_run: bool,
}

/// Resolve a target to stack ids. A query and explicit paths are mutually
/// exclusive; requiring one of them prevents an unqualified command from
/// silently matching the entire library.
fn resolve(cat: &Catalog, t: &Target) -> Result<Vec<i64>> {
    if let Some(q) = &t.query {
        let compiled = npc::query::compile(q)?;
        let stmt = compiled.select_stacks(None);
        let mut prepared = cat.conn.prepare(&stmt)?;
        let params: Vec<&dyn rusqlite::ToSql> = compiled
            .params
            .iter()
            .map(|p| p as &dyn rusqlite::ToSql)
            .collect();
        let ids = prepared
            .query_map(params.as_slice(), |r| r.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(ids);
    }

    if t.paths.is_empty() {
        anyhow::bail!(
            "nothing selected: pass -q '<query>' or one or more stack paths.\n\
             Refusing to act on the whole library by default."
        );
    }

    let mut ids = Vec::new();
    for p in &t.paths {
        let path = Path::new(p);
        let dir = path
            .parent()
            .map(|d| d.to_string_lossy().to_string())
            .unwrap_or_default();
        let base = path
            .file_stem()
            .map(|b| b.to_string_lossy().to_string())
            .unwrap_or_default();
        match cat.stack_id(&dir, &base)? {
            Some(id) => ids.push(id),
            None => anyhow::bail!("no stack in the catalog for {p}"),
        }
    }
    Ok(ids)
}

/// `dir` must resolve to somewhere inside `root` -- checked before `npc scan
/// --only` walks it, so a relative path or a `..` can't make an unrelated
/// directory look like it's inside the library. Canonicalizing both first is
/// what makes that check meaningful rather than a string-prefix guess; same
/// reasoning `import::plan`'s own overlap check uses.
///
/// Returns the directory spelled under `root` as given, so `--only .` or
/// `--only ../x` is cataloged with the same dirs a full scan stores.
fn only_dir_is_inside_library(root: &Path, dir: &Path) -> Result<PathBuf> {
    let root_canon = root
        .canonicalize()
        .with_context(|| format!("resolving library {}", root.display()))?;
    let dir_canon = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    anyhow::ensure!(
        dir_canon.starts_with(&root_canon),
        "{} is not inside the library {} -- --only takes a subdirectory of \
         it, not an arbitrary path",
        dir.display(),
        root.display()
    );
    let rel = dir_canon.strip_prefix(&root_canon).unwrap_or(Path::new(""));
    Ok(root.join(rel))
}

/// Every stack currently cataloged under one of `dirs`, by exact match --
/// used to hand `import`'s background thumbnail warm-up an exact id list
/// for what it just copied, without going through the query language's
/// `path=` (a substring match, imprecise for this purpose).
fn stack_ids_in_dirs(cat: &Catalog, dirs: &std::collections::HashSet<PathBuf>) -> Result<Vec<i64>> {
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    let dir_strings: Vec<String> = dirs
        .iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect();
    let placeholders = dir_strings
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT id FROM stack WHERE dir IN ({placeholders})");
    let params: Vec<&dyn rusqlite::ToSql> = dir_strings
        .iter()
        .map(|d| d as &dyn rusqlite::ToSql)
        .collect();
    let mut stmt = cat.conn.prepare(&sql)?;
    let ids: Vec<i64> = stmt
        .query_map(params.as_slice(), |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// The library to act on: `--root` if given, else `library` from the config.
///
/// Deliberately errors rather than defaulting to the current directory --
/// scanning whatever happens to be underfoot is not a helpful guess.
fn library(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(r) = explicit {
        // `npc scan .` must not record `.` as the library's root.
        return std::path::absolute(r).with_context(|| format!("resolving {}", r.display()));
    }
    if let Some(r) = npc::config::Config::load()
        .unwrap_or_default()
        .library_root()
    {
        return Ok(r);
    }
    anyhow::bail!(
        "no library given. Pass --root <path>, or set one in {}:\n\n    \
         library = \"~/Pictures\"",
        npc::config::Config::path().display()
    )
}

fn export_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(o) = explicit {
        return Ok(o.to_path_buf());
    }
    if let Some(o) = npc::config::Config::load()
        .unwrap_or_default()
        .default_export_dir()
    {
        return Ok(o);
    }
    anyhow::bail!(
        "no export destination given. Pass -o/--out <path>, or set one in {}:\n\n    \
         export_dir = \"~/Pictures/graded\"",
        npc::config::Config::path().display()
    )
}

fn open_for(root: &Path) -> Result<Catalog> {
    Ok(open_checked(root)?.0)
}

/// Detect mounted cards and prompt for one or more to import. `Ok(None)`
/// means the user cancelled (blank input) -- not an error, just nothing to do.
fn pick_cards() -> Result<Option<Vec<PathBuf>>> {
    let cards = npc::cards::detect()?;
    if cards.is_empty() {
        anyhow::bail!(
            "no mounted card with a DCIM folder was found. \
             Pass a path explicitly: npc import <path> --library <path>"
        );
    }

    println!("mounted cards:");
    for (i, c) in cards.iter().enumerate() {
        match &c.label {
            Some(label) => println!("  {}) {} ({label})", i + 1, c.mount.display()),
            None => println!("  {}) {}", i + 1, c.mount.display()),
        }
    }
    print!("import which? (e.g. \"1,3\", \"all\", enter to cancel): ");
    std::io::Write::flush(&mut std::io::stdout())?;

    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;

    let picked = npc::cards::parse_selection(&line, cards.len())?;
    if picked.is_empty() {
        println!("nothing selected");
        return Ok(None);
    }

    Ok(Some(
        picked.into_iter().map(|i| cards[i].mount.clone()).collect(),
    ))
}

/// Open a catalog and say whether the library itself is reachable. Read-only
/// commands work either way; anything that touches files must check.
fn open_checked(root: &Path) -> Result<(Catalog, bool)> {
    let (cat, online) = Catalog::open_for_root(root)?;
    if !online {
        eprintln!(
            "OFFLINE  {} is not mounted -- answering from the catalog.",
            root.display()
        );
    }
    Ok((cat, online))
}

/// Refuse an operation that needs the actual files.
fn require_online(root: &Path) -> Result<Catalog> {
    let (cat, online) = open_checked(root)?;
    anyhow::ensure!(
        online,
        "{} is not mounted, and this needs the files themselves. \
         Reconnect the drive and try again.",
        root.display()
    );
    Ok(cat)
}

/// Apply changes, then immediately flush. Edits are useless sitting in the
/// catalog: the files on disk are the source of truth.
fn edit_and_flush(t: &Target, changes: &[npc::edit::Change], verb: &str) -> Result<()> {
    let config = npc::config::Config::load()?;
    let root = library(t.root.as_deref())?;
    let mut cat = open_for(&root)?;
    let ids = resolve(&cat, t)?;

    if ids.is_empty() {
        println!("nothing matched");
        return Ok(());
    }
    if t.dry_run {
        println!("would {verb} {} stack(s)", ids.len());
        return Ok(());
    }

    npc::edit::apply(&mut cat, &ids, changes, &config)?;
    let stats = npc::edit::flush(&mut cat, &config)?;

    println!("{verb} {} stack(s)", ids.len());
    println!("  sidecars written  {}", stats.sidecars_written);
    if stats.queued_offline > 0 {
        println!("  queued (offline)  {}", stats.queued_offline);
    }
    for e in &stats.errors {
        eprintln!("  error: {e}");
    }
    Ok(())
}

fn stars(rating: Option<i64>) -> String {
    let n = usize::try_from(rating.unwrap_or(0).clamp(0, 5)).unwrap_or(0);
    format!("{}{}", "*".repeat(n), " ".repeat(5 - n))
}

/// A fixed-width `[####----]` bar for `npc import`'s copy phase -- plain
/// ASCII and a hand-rolled fill rather than a progress-bar crate, the same
/// "a dozen lines doesn't need a dependency" call this project already made
/// for `tui.rs`'s base64 encoder. `frac` is clamped so a rounding wobble at
/// 100% (`bytes_so_far` can briefly exceed `total_bytes` by a few bytes of
/// container overhead) never prints a bar wider than `width`.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "frac is clamped to [0,1] and width is a handful of characters; \
              this never approaches a range where the cast loses anything"
)]
fn progress_bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("[{}{}]", "#".repeat(filled), "-".repeat(width - filled))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Thumbnail sizes come from config; the cache key includes them, so
    // changing them invalidates exactly the affected entries.
    if let Ok(cfg) = npc::config::Config::load() {
        npc::thumbs::configure(&cfg.thumbs);
    }

    match cli.cmd {
        Cmd::Probe { path, preview } => {
            let m = meta::read_meta(&path)?;
            println!("file        {path}");
            println!("  camera    {}", m.camera);
            println!("  lens      {}", m.lens);
            println!("  captured  {} (unix)", m.capture_time);
            println!("  size      {}x{}", m.width, m.height);
            println!("  iso       {}", m.iso);
            println!("  aperture  f/{:.1}", m.fnumber);
            println!("  exposure  {:.5}s", m.exposure);
            println!("  focal     {:.0}mm", m.focal);
            println!("  orient    {}", m.orientation);
            println!("  serial    {}", m.serial);
            println!("  shutter   {}", m.shutter_count);
            println!("  rating    {}", m.rating);
            println!("  tags      {:?}", m.tags);

            if let Some(min_dim) = preview {
                let p = meta::extract_preview(&path, min_dim)?;
                if p.ok {
                    println!(
                        "  preview   {}x{} {} ({} bytes) for min_dim={min_dim}",
                        p.width,
                        p.height,
                        p.mime,
                        p.data.len()
                    );
                } else {
                    println!("  preview   none ({})", p.error);
                }
            }
        }

        Cmd::Volume { path } => {
            let v = vol::identify(&path)?;
            println!("volume    {}", v.uuid);
            println!("  label   {}", v.label.unwrap_or_else(|| "-".into()));
            println!("  device  {}", v.device.unwrap_or_else(|| "-".into()));
            println!("  mount   {}", v.mount.display());
            if !v.stable {
                println!("  note    no filesystem UUID found; using the device number,");
                println!("          which is not stable across reboots or replugs");
            }
        }

        Cmd::Scan { root, full, only } => {
            let root = library(root.get().as_deref())?;
            let started = std::time::Instant::now();

            let (stats, cat_path, scanned) = if only.is_empty() {
                let (stats, cat) = scan::scan_root(&root, full)?;
                (stats, cat.path, root.display().to_string())
            } else {
                // Checked before canonicalizing: an unmounted drive fails a
                // path-resolution with a confusing message, when the real
                // problem is simpler and already has its own clear one.
                let mut cat = require_online(&root)?;

                let dirs = only
                    .iter()
                    .map(|dir| only_dir_is_inside_library(&root, dir))
                    .collect::<Result<Vec<_>>>()?;

                if let Ok(vol) = npc::vol::identify(&root) {
                    cat.register_volume(&vol.uuid, vol.label.as_deref(), &root)?;
                }
                let mut totals = scan::ScanStats::default();
                for dir in &dirs {
                    let s = scan::scan(&mut cat, dir, full).context("scanning")?;
                    totals.files_seen += s.files_seen;
                    totals.unchanged += s.unchanged;
                    totals.parsed += s.parsed;
                    totals.stacks += s.stacks;
                    totals.orphans += s.orphans;
                    totals.vanished += s.vanished;
                    totals.sidecars_read += s.sidecars_read;
                    totals.events += s.events;
                    totals.seeded_from_embedded += s.seeded_from_embedded;
                    totals.inherited_rating += s.inherited_rating;
                    totals.errors.extend(s.errors);
                }
                let names = only
                    .iter()
                    .map(|d| d.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                (totals, cat.path.clone(), names)
            };
            let elapsed = started.elapsed();

            println!("scanned {scanned}");
            println!("  catalog     {}", cat_path.display());
            println!("  files seen  {}", stats.files_seen);
            println!("  stacks      {}", stats.stacks);
            println!("  parsed      {}", stats.parsed);
            println!("  unchanged   {}", stats.unchanged);
            println!("  orphans     {}", stats.orphans);
            if stats.seeded_from_embedded > 0 {
                println!(
                    "  seeded      {} stack(s) from embedded rating/tags (run `npc sync` to write sidecars)",
                    stats.seeded_from_embedded
                );
            }
            if stats.inherited_rating > 0 {
                println!(
                    "  inherited   {} stack(s) rated from a same-named stack elsewhere (run `npc sync` to write sidecars)",
                    stats.inherited_rating
                );
            }
            if stats.vanished > 0 {
                println!(
                    "  vanished    {} (marked absent, tags kept)",
                    stats.vanished
                );
            }
            if !stats.errors.is_empty() {
                println!("  errors      {}", stats.errors.len());
                for e in stats.errors.iter().take(10) {
                    println!("    {e}");
                }
                if stats.errors.len() > 10 {
                    println!("    ... and {} more", stats.errors.len() - 10);
                }
            }
            let rate = stats.files_seen as f64 / elapsed.as_secs_f64().max(1e-9);
            println!(
                "  took        {:.2}s  ({rate:.0} files/s)",
                elapsed.as_secs_f64()
            );
        }

        Cmd::Find {
            query,
            root,
            files,
            print0,
            limit,
            explain,
            help_syntax,
        } => {
            if help_syntax {
                println!("{}", npc::query::SYNTAX);
                return Ok(());
            }
            let compiled = npc::query::compile(&query)?;
            let stmt = compiled.select_stacks(limit);

            if explain {
                println!("{stmt}");
                println!("\n-- parameters:");
                for (i, p) in compiled.params.iter().enumerate() {
                    println!("--   ?{} = {:?}", i + 1, p);
                }
                return Ok(());
            }

            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            let mut prepared = cat.conn.prepare(&stmt)?;
            let params: Vec<&dyn rusqlite::ToSql> = compiled
                .params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();

            // id, dir, base, rating, flag, reject, captured
            #[allow(
                clippy::items_after_statements,
                reason = "the alias documents the row shape where it is read"
            )]
            type FoundStack = (i64, String, String, Option<i64>, i64, i64, Option<i64>);
            let rows: Vec<FoundStack> = prepared
                .query_map(params.as_slice(), |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                })?
                .collect::<Result<_, _>>()?;

            let sep = if print0 { '\0' } else { '\n' };

            for (id, dir, base, rating, flag, reject, _captured) in &rows {
                if files {
                    let mut fq = cat.conn.prepare(
                        "SELECT name FROM file WHERE stack_id = ?1 AND present = 1 ORDER BY role, name",
                    )?;
                    for name in fq.query_map([id], |r| r.get::<_, String>(0))? {
                        print!("{dir}/{}{sep}", name?);
                    }
                } else if print0 {
                    print!("{dir}/{base}{sep}");
                } else {
                    let marks = format!(
                        "{}{}",
                        if *flag == 1 { "F" } else { " " },
                        if *reject == 1 { "X" } else { " " }
                    );
                    println!("{} {marks}  {dir}/{base}", stars(*rating));
                }
            }

            if !print0 && !files {
                eprintln!("\n{} stack(s)", rows.len());
            }
        }

        Cmd::Tags { root } => {
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            let mut stmt = cat.conn.prepare(
                "SELECT t.name, COUNT(st.stack_id) AS n
                 FROM tag t LEFT JOIN stack_tag st ON st.tag_id = t.id
                 GROUP BY t.id ORDER BY n DESC, t.name",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            let mut any = false;
            for row in rows {
                let (name, n) = row?;
                println!("{n:>6}  {name}");
                any = true;
            }
            if !any {
                println!("no tags yet (tagging arrives in M2)");
            }
        }

        Cmd::Tag {
            action,
            names,
            target,
        } => {
            let list: Vec<String> = names
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            anyhow::ensure!(!list.is_empty(), "no tag names given");
            let change = match action.as_str() {
                "add" => npc::edit::Change::AddTags(list),
                "rm" | "remove" => npc::edit::Change::RemoveTags(list),
                other => anyhow::bail!("unknown tag action `{other}`; use `add` or `rm`"),
            };
            edit_and_flush(&target, &[change], &format!("tag {action}"))?;
        }

        Cmd::Rate { value, target } => {
            let rating = if value == "-" || value == "clear" {
                None
            } else {
                let n: i64 = value.parse().context("rating must be 0-5 or `-`")?;
                anyhow::ensure!((0..=5).contains(&n), "rating must be between 0 and 5");
                Some(n)
            };
            edit_and_flush(&target, &[npc::edit::Change::SetRating(rating)], "rated")?;
        }

        Cmd::RateSync { target } => {
            let config = npc::config::Config::load()?;
            let root = library(target.root.as_deref())?;
            let mut cat = open_for(&root)?;
            let ids = resolve(&cat, &target)?;
            if ids.is_empty() {
                println!("nothing matched");
                return Ok(());
            }

            let candidates: Vec<_> = npc::metadata_sync::scan_ids(&cat, &ids)?
                .into_iter()
                .filter(npc::metadata_sync::Comparison::needs_write)
                .collect();

            if candidates.is_empty() {
                println!("{} stack(s) checked, all already resolved", ids.len());
                return Ok(());
            }

            for c in &candidates {
                let mut parts = Vec::new();
                if c.needs_rating_write() {
                    parts.push(format!(
                        "rating {}->{}",
                        c.catalog_rating.map_or("-".to_string(), |v| v.to_string()),
                        c.resolved_rating
                            .expect("needs_rating_write implies resolved_rating is Some")
                    ));
                }
                for t in c.added_tags() {
                    parts.push(format!("+tag {t}"));
                }
                println!(
                    "{}{}/{}  {}",
                    if target.dry_run {
                        "would sync  "
                    } else {
                        "syncing     "
                    },
                    c.dir,
                    c.base,
                    parts.join(", "),
                );
            }

            if target.dry_run {
                println!("would resync {} stack(s)", candidates.len());
                return Ok(());
            }

            for c in &candidates {
                let mut changes = Vec::new();
                if c.needs_rating_write() {
                    changes.push(npc::edit::Change::SetRating(
                        c.resolved_rating.map(i64::from),
                    ));
                }
                let added = c.added_tags();
                if !added.is_empty() {
                    changes.push(npc::edit::Change::AddTags(added));
                }
                npc::edit::apply(&mut cat, &[c.stack_id], &changes, &config)?;
            }
            let stats = npc::edit::flush(&mut cat, &config)?;

            println!("resynced {} stack(s)", candidates.len());
            println!("  sidecars written  {}", stats.sidecars_written);
            if stats.queued_offline > 0 {
                println!("  queued (offline)  {}", stats.queued_offline);
            }
            for e in &stats.errors {
                eprintln!("  error: {e}");
            }
        }

        Cmd::Mark { state, target } => {
            let changes = match state.as_str() {
                "flag" => vec![npc::edit::Change::SetFlag(true)],
                "reject" => vec![npc::edit::Change::SetReject(true)],
                "clear" => vec![
                    npc::edit::Change::SetFlag(false),
                    npc::edit::Change::SetReject(false),
                ],
                other => anyhow::bail!("unknown state `{other}`; use flag, reject or clear"),
            };
            edit_and_flush(&target, &changes, &format!("marked {state}"))?;
        }

        Cmd::Set {
            field,
            value,
            target,
        } => {
            let change = match field.as_str() {
                "title" => npc::edit::Change::SetTitle(value),
                "description" | "desc" => npc::edit::Change::SetDescription(value),
                other => anyhow::bail!("unknown field `{other}`; use title or description"),
            };
            edit_and_flush(&target, &[change], &format!("set {field} on"))?;
        }

        Cmd::Rotate { direction, target } => {
            // EXIF orientation values, as a quarter-turn each way. Composing
            // with whatever is already set would need the current value per
            // stack; setting absolutely is predictable and matches how every
            // viewer treats the tag.
            let value: u16 = match direction.as_str() {
                "cw" | "right" => 6, // rotate 90 CW
                "ccw" | "left" => 8, // rotate 270 CW
                "180" | "flip" => 3,
                "none" | "clear" | "0" => 1,
                other => anyhow::bail!("unknown direction `{other}`; use cw, ccw, 180 or none"),
            };
            edit_and_flush(
                &target,
                &[npc::edit::Change::SetOrientation(value)],
                &format!("rotated {direction}"),
            )?;
            println!(
                "  note      orientation is written to the sidecar only, so \n\
                 \x20           viewers that ignore sidecars will still show the \n\
                 \x20           original framing. See PLAN.md."
            );
        }

        Cmd::Exec {
            action,
            query,
            root,
            list,
            dry_run,
        } => {
            let actions = npc::action::load();
            if list || action.is_empty() {
                if actions.is_empty() {
                    println!("no actions available (none of the configured programs are on PATH)");
                } else {
                    println!("{:<4} {:<14} command", "key", "name");
                    for a in &actions {
                        println!("{:<4} {:<14} {}", a.key, a.name, a.cmd.join(" "));
                    }
                }
                return Ok(());
            }

            let chosen = actions
                .iter()
                .find(|a| a.name.eq_ignore_ascii_case(&action) || a.key == action)
                .ok_or_else(|| anyhow::anyhow!("no action `{action}`; try `npc exec --list`"))?;

            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            let target = Target {
                query: query.clone(),
                paths: Vec::new(),
                root: Some(root.clone()),
                dry_run,
            };
            let ids = resolve(&cat, &target)?;
            if ids.is_empty() {
                println!("nothing matched");
                return Ok(());
            }

            // Same rule as the TUI: an external editor cannot be prevented
            // from writing, so a protected root must not be handed to one.
            for &sid in &ids {
                let dir: String =
                    cat.conn
                        .query_row("SELECT dir FROM stack WHERE id = ?1", [sid], |r| r.get(0))?;
                config.ensure_mutable(Path::new(&dir)).with_context(|| {
                    "refusing to launch an external program against a protected root"
                })?;
            }

            let sel =
                npc::action::gather(&cat, &ids, ids.first().copied(), &query.unwrap_or_default())?;
            let argv = npc::action::build_argv(chosen, &sel)?;

            if dry_run {
                println!("would run:");
                for a in &argv {
                    println!("  {a}");
                }
                return Ok(());
            }
            let outcome = npc::action::run(chosen, &sel, &config)?;
            println!("{}", outcome.description);
            if let Some(text) = outcome.output {
                if !text.is_empty() {
                    println!("{text}");
                }
            }
        }

        Cmd::Sync { root } => {
            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;

            let mut cat = open_for(&root)?;
            let stats = npc::edit::flush(&mut cat, &config)?;
            println!("sidecars written  {}", stats.sidecars_written);
            if stats.queued_offline > 0 {
                println!("queued (offline)  {}", stats.queued_offline);
            }
            for e in &stats.errors {
                eprintln!("error: {e}");
            }
        }

        Cmd::Import {
            source,
            library: library_arg,
            dry_run,
            verify_content,
            rescan,
            deep,
        } => {
            let root = library(library_arg.as_deref())?;
            let config = npc::config::Config::load()?;
            config.ensure_mutable(&root)?;
            // Importing into a library that does not exist yet is the normal
            // first run, and must not fail on volume resolution.
            std::fs::create_dir_all(&root)
                .with_context(|| format!("creating library {}", root.display()))?;

            // Dedup only ever sees what the catalog knows. Planning against a
            // stale one (a file added by hand, or just time since the last
            // scan) can silently miss a real duplicate and create an unwanted
            // copy -- confirmed live: a file already in the library was
            // planned as new because its directory had never been scanned in
            // at all. `--rescan` re-establishes that freshness guarantee by
            // walking the whole library first; by default the catalog is
            // trusted as-is, which is far faster but carries that same risk
            // if something outside npc touched the library meanwhile.
            let mut cat = if rescan {
                println!("scanning {}...", root.display());
                let (scan_stats, cat) = scan::scan_root(&root, false)?;
                println!(
                    "catalog: {} stacks ({} new/changed)",
                    scan_stats.stacks, scan_stats.parsed
                );
                cat
            } else {
                let cat = require_online(&root)?;
                println!(
                    "catalog: {} stacks (not rescanned; pass --rescan to verify first)",
                    cat.stack_count()?
                );
                cat
            };

            let sources = match source {
                Some(p) => vec![p],
                None => match pick_cards()? {
                    Some(picked) => picked,
                    None => return Ok(()),
                },
            };

            let mut totals = npc::import::ImportStats::default();
            // Every destination directory a stack actually landed in this
            // run -- used once every card is cataloged (below) to resolve
            // exactly which stacks to prewarm thumbnails for, without
            // touching anything import.rs itself returns.
            let mut touched_dirs: std::collections::HashSet<PathBuf> =
                std::collections::HashSet::new();
            for source in &sources {
                println!("scanning {}...", source.display());
                let plans = npc::import::plan(&cat, source, &root, verify_content, deep)?;
                let dupes = plans.iter().filter(|p| p.duplicate_of.is_some()).count();
                let fresh = plans.len() - dupes;
                let unread = plans
                    .iter()
                    .filter(|p| p.duplicate_of.is_some() && p.already.iter().all(|a| a.unread))
                    .count();
                let joining = plans.iter().filter(|p| p.joins_existing).count();

                println!("import {} -> {}", source.display(), root.display());
                println!("  stacks found      {}", plans.len());
                println!("  already in library {dupes}");
                if unread > 0 {
                    println!(
                        "    of which {unread} matched in the catalog by name and size, not read \
                         (--deep reads them)"
                    );
                }
                println!("  to copy           {fresh}");
                if joining > 0 {
                    println!("    of which {joining} add a missing RAW/JPG to a stack already in the library");
                }

                let mut by_day: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::default();
                for p in plans.iter().filter(|p| p.duplicate_of.is_none()) {
                    *by_day
                        .entry(
                            p.dest_dir
                                .strip_prefix(&root)
                                .unwrap_or(&p.dest_dir)
                                .to_string_lossy()
                                .to_string(),
                        )
                        .or_default() += 1;
                }
                for (day, n) in &by_day {
                    println!("    {day:<14} {n} stack(s)");
                }

                if verify_content {
                    for p in &plans {
                        let Some(nm) = &p.near_miss else { continue };
                        println!(
                            "near-miss (identity+size matched, content didn't -- importing as new)\n  \
                             card:    {}\n  \
                             library: {}\n  \
                             {} byte(s) differ, first at offset {}",
                            nm.source, nm.candidate, nm.diff_bytes, nm.first_diff_offset
                        );
                    }
                }

                let total_bytes: u64 = plans
                    .iter()
                    .filter(|p| p.duplicate_of.is_none())
                    .flat_map(|p| p.members.iter())
                    .map(|m| m.size)
                    .sum();

                println!();
                let mut bytes_so_far = 0u64;
                let stats = npc::import::execute(&plans, &mut cat, &config, dry_run, &mut |ev| {
                    match ev {
                        npc::import::ImportEvent::Skipped(stack) => {
                            let name = stack.already.first().map_or_else(
                                || stack.base.clone(),
                                |a| a.src.display().to_string(),
                            );
                            let existing = stack.duplicate_of.as_deref().unwrap_or("?");
                            match stack.metadata_merge.as_ref().filter(|m| m.needs_write()) {
                                Some(m) => {
                                    let verb = if dry_run {
                                        "would merge metadata"
                                    } else {
                                        "merged metadata"
                                    };
                                    let mut parts = Vec::new();
                                    if m.changes_rating() {
                                        parts.push(format!(
                                            "rating {}->{}",
                                            m.rating_before
                                                .map_or("-".to_string(), |v| v.to_string()),
                                            m.rating_after.unwrap()
                                        ));
                                    }
                                    for t in m.added_tags() {
                                        parts.push(format!("+tag {t}"));
                                    }
                                    println!("{verb}: {name} -> {existing}  {}", parts.join(", "));
                                }
                                None => {
                                    println!("skip (already in library, unchanged): {name} -> {existing}");
                                }
                            }
                        }
                        npc::import::ImportEvent::Copied {
                            index,
                            total,
                            stack,
                            bytes,
                        } => {
                            if dry_run {
                                // Nothing is actually copied yet -- show exactly
                                // where each member would land, so the plan is
                                // inspectable before committing to it.
                                for m in &stack.members {
                                    println!(
                                        "  would copy: {} -> {}",
                                        m.src.display(),
                                        m.dest.display()
                                    );
                                }
                            } else {
                                touched_dirs.insert(stack.dest_dir.clone());
                                bytes_so_far += bytes;
                                let frac = if total_bytes > 0 {
                                    bytes_so_far as f64 / total_bytes as f64
                                } else {
                                    1.0
                                };
                                print!(
                                    "\r{} {:3.0}%  {index}/{total} stacks  {:.1}/{:.1} GB\x1b[K",
                                    progress_bar(frac, 30),
                                    frac * 100.0,
                                    bytes_so_far as f64 / 1e9,
                                    total_bytes as f64 / 1e9
                                );
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                        }
                        npc::import::ImportEvent::Failed { error, .. } => {
                            eprintln!("\nerror: {error}");
                        }
                    }
                })?;
                println!();

                if dry_run {
                    println!(
                        "would copy {} files, {:.1} GB",
                        stats.files_copied,
                        stats.bytes_copied as f64 / 1e9
                    );
                } else {
                    println!(
                        "copied {} stacks, {} files, {:.1} GB (source untouched)",
                        stats.stacks_copied,
                        stats.files_copied,
                        stats.bytes_copied as f64 / 1e9
                    );
                    if stats.duplicates_skipped > 0 {
                        println!("skipped {} duplicate(s)", stats.duplicates_skipped);
                    }
                    if stats.merges_applied > 0 {
                        println!("merged metadata into {} stack(s)", stats.merges_applied);
                    }
                }
                println!();

                if !dry_run {
                    // Catalog this card's copies now, not only at the end: the
                    // next card is planned against the catalog, and a RAW from
                    // card 2 can only find its JPG from card 1 there -- without
                    // it, the RAW would be filed as a different shot under a
                    // suffixed name. Scoped to the folders this card wrote to,
                    // the way the watcher rescans one.
                    let dirs: std::collections::BTreeSet<&PathBuf> = plans
                        .iter()
                        .filter(|p| !p.members.is_empty())
                        .map(|p| &p.dest_dir)
                        .collect();
                    for dir in dirs {
                        if dir.is_dir() {
                            scan::scan(&mut cat, dir, false)
                                .with_context(|| format!("cataloging {}", dir.display()))?;
                        }
                    }
                    npc::import::record_sources(&mut cat, &stats.sources)?;
                }

                totals.stacks_copied += stats.stacks_copied;
                totals.files_copied += stats.files_copied;
                totals.bytes_copied += stats.bytes_copied;
                totals.duplicates_skipped += stats.duplicates_skipped;
                totals.merges_applied += stats.merges_applied;
                totals.errors.extend(stats.errors);
            }

            if !dry_run {
                // No full-library scan here: each card's folders were cataloged
                // right after it was copied (above), which is everything this
                // import changed. A walk of the whole library cost ~10 min per
                // import to find nothing more; `npc scan` is for the rest.
                println!("catalog now holds {} stacks", cat.stack_count()?);

                // Thumbnails for what just landed: warm the Grid tier in a
                // detached background process rather than making `import`
                // wait, or leaving every tile a cold cache miss the first
                // time this import is browsed. Resolved by exact directory
                // match against what was actually copied above, not the
                // query language's `path=` (a substring match) -- this is
                // an internal handoff, not a user-typed filter.
                if !touched_dirs.is_empty() {
                    let ids = stack_ids_in_dirs(&cat, &touched_dirs)?;

                    if !ids.is_empty() {
                        let ids_path = std::env::temp_dir().join(format!(
                            "npc-prewarm-{}-{}.ids",
                            std::process::id(),
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or_default()
                        ));
                        let body = ids
                            .iter()
                            .map(i64::to_string)
                            .collect::<Vec<_>>()
                            .join("\n");
                        std::fs::write(&ids_path, body)
                            .with_context(|| format!("writing {}", ids_path.display()))?;

                        let spawned = std::env::current_exe().and_then(|exe| {
                            std::process::Command::new(exe)
                                .arg("thumbs")
                                .arg("--root")
                                .arg(&root)
                                .arg("--size")
                                .arg("grid")
                                .arg("--ids-file")
                                .arg(&ids_path)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn()
                        });
                        match spawned {
                            // Deliberately not `.wait()`-ed: on Linux an
                            // un-waited child is reparented to init when
                            // this process exits normally, not killed. That
                            // is what lets it keep generating thumbnails
                            // after `import` itself has already returned.
                            Ok(child) => {
                                drop(child);
                                println!("warming {} thumbnail(s) in the background", ids.len());
                            }
                            Err(e) => {
                                eprintln!(
                                    "note: could not start background thumbnail warm-up: {e}"
                                );
                                let _ = std::fs::remove_file(&ids_path);
                            }
                        }
                    }
                }
            }
            if sources.len() > 1 {
                println!(
                    "\ntotals: {} stacks, {} files, {:.1} GB, {} duplicate(s) skipped",
                    totals.stacks_copied,
                    totals.files_copied,
                    totals.bytes_copied as f64 / 1e9,
                    totals.duplicates_skipped
                );
            }
        }

        Cmd::Events { root, name } => {
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;

            if let Some(args) = name {
                let id: i64 = args[0].parse().context("event id must be a number")?;
                let label = args[1].trim();
                let value = (!label.is_empty()).then_some(label);
                let n = cat.conn.execute(
                    "UPDATE event SET name = ?2, auto = 0 WHERE id = ?1",
                    rusqlite::params![id, value],
                )?;
                anyhow::ensure!(n == 1, "no event with id {id}");
                println!(
                    "event {id} {}",
                    match value {
                        Some(v) => format!("named `{v}`"),
                        None => "name cleared".into(),
                    }
                );
                return Ok(());
            }

            let mut stmt = cat.conn.prepare(
                "SELECT e.id, date(e.start_time,'unixepoch'), COALESCE(e.name,''),
                        COUNT(s.id)
                 FROM event e LEFT JOIN stack s ON s.event_id = e.id
                 GROUP BY e.id ORDER BY e.start_time DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            let mut any = false;
            for row in rows {
                let (id, day, name, n) = row?;
                println!("{id:>4}  {day}  {n:>6} stacks   {name}");
                any = true;
            }
            if any {
                println!("\nname one with: npc events --name <id> \"<name>\"");
            } else {
                println!("no events yet; run `npc scan` first");
            }
        }

        Cmd::Doctor { root, strict } => {
            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            println!("{}\n", cat.path.display());

            let findings = npc::doctor::run(&cat, &root, &config)?;
            let mut warnings = 0;

            for f in &findings {
                println!("{}  {}", f.severity.marker(), f.title);
                for line in &f.detail {
                    println!("        {line}");
                }
                if f.severity != npc::doctor::Severity::Fine {
                    if let Some(r) = &f.remedy {
                        // Wrap the remedy so it reads as prose rather than one
                        // very long line.
                        let mut col = 0;
                        print!("        ");
                        for word in r.split_whitespace() {
                            if col + word.len() > 66 {
                                print!("\n        ");
                                col = 0;
                            }
                            print!("{word} ");
                            col += word.len() + 1;
                        }
                        println!();
                    }
                }
                if f.severity == npc::doctor::Severity::Warn {
                    warnings += 1;
                }
                println!();
            }

            if warnings == 0 {
                println!("nothing needs attention");
            } else {
                println!("{warnings} thing(s) need attention");
            }
            if strict && warnings > 0 {
                std::process::exit(1);
            }
        }

        Cmd::Dupes { root } => {
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            let groups = npc::dupes::find(&cat)?;
            if groups.is_empty() {
                println!("no duplicates found");
            }
            for g in &groups {
                println!("{}", g.reason);
                for p in &g.paths {
                    println!("    {p}");
                }
            }
            if !groups.is_empty() {
                println!("\n{} duplicate group(s)", groups.len());
            }
        }

        Cmd::Trash { target } => {
            let config = npc::config::Config::load()?;
            let root = library(target.root.as_deref())?;
            let mut cat = require_online(&root)?;
            let ids = resolve(&cat, &target)?;
            if ids.is_empty() {
                println!("nothing matched");
                return Ok(());
            }
            if target.dry_run {
                println!("would trash {} stack(s)", ids.len());
                return Ok(());
            }
            let stats = npc::trash::trash(&mut cat, &root, &ids, &config)?;
            println!(
                "trashed {} stack(s), {} files -> {}/.trash/",
                stats.stacks,
                stats.files,
                root.display()
            );
            println!("restore with `npc untrash <base>`; nothing was deleted");
            for e in &stats.errors {
                eprintln!("error: {e}");
            }
        }

        Cmd::Untrash { base, root } => {
            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;

            let mut cat = open_for(&root)?;
            let entry = npc::trash::list(&cat)?
                .into_iter()
                .find(|e| e.base == base)
                .ok_or_else(|| anyhow::anyhow!("`{base}` is not in the trash"))?;
            let n = npc::trash::restore(&mut cat, entry.stack_id, &config)?;
            println!("restored {base}: {n} files -> {}", entry.orig_dir);
        }

        Cmd::TrashList { root } => {
            let root = library(root.as_deref())?;

            let cat = open_for(&root)?;
            let entries = npc::trash::list(&cat)?;
            if entries.is_empty() {
                println!("trash is empty");
            }
            for e in &entries {
                println!("{:<24} from {}", e.base, e.orig_dir);
            }
            if !entries.is_empty() {
                println!("\n{} stack(s) in the trash", entries.len());
            }
        }

        Cmd::TrashEmpty { root, yes } => {
            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;

            let mut cat = open_for(&root)?;
            let entries = npc::trash::list(&cat)?;
            if entries.is_empty() {
                println!("trash is empty");
                return Ok(());
            }
            if !yes {
                println!("{} stack(s) would be permanently deleted:", entries.len());
                for e in entries.iter().take(10) {
                    println!("    {}", e.base);
                }
                if entries.len() > 10 {
                    println!("    ... and {} more", entries.len() - 10);
                }
                println!("\nThis cannot be undone. Re-run with --yes to proceed.");
                return Ok(());
            }
            let stats = npc::trash::empty(&mut cat, &config)?;
            println!("deleted {} stack(s), {} files", stats.stacks, stats.files);
            for e in &stats.errors {
                eprintln!("error: {e}");
            }
        }

        Cmd::Thumbs {
            root,
            query,
            size,
            prune,
            ids_file,
        } => {
            let size = match size.as_str() {
                "grid" => npc::thumbs::Size::Grid,
                "view" => npc::thumbs::Size::View,
                other => anyhow::bail!("unknown size `{other}`; use grid or view"),
            };
            // Generating needs the originals; pruning does not.
            let root = library(root.as_deref())?;

            let (cat, online) = open_checked(&root)?;
            let uuid: String = cat
                .conn
                .query_row("SELECT vol_uuid FROM volume LIMIT 1", [], |r| r.get(0))
                .unwrap_or_default();
            anyhow::ensure!(
                online || prune,
                "{} is not mounted; thumbnails need the originals. \
                 (`--prune` works offline.)",
                root.display()
            );
            let cache = npc::thumbs::Cache::new(&uuid)?;

            // An explicit id list (from `import`'s background handoff) skips
            // the query compiler entirely -- these ids are already known
            // exactly, and running them back through the query language
            // would just be a slower, lossier way to say the same thing.
            let ids: Vec<i64> = if let Some(path) = &ids_file {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let ids = text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        l.trim().parse::<i64>().with_context(|| {
                            format!("parsing stack id from {}: {l:?}", path.display())
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let _ = std::fs::remove_file(path);
                ids
            } else {
                let compiled = npc::query::compile(query.as_deref().unwrap_or(""))?;
                let stmt = compiled.select_stacks(None);
                let mut prepared = cat.conn.prepare(&stmt)?;
                let params: Vec<&dyn rusqlite::ToSql> = compiled
                    .params
                    .iter()
                    .map(|p| p as &dyn rusqlite::ToSql)
                    .collect();
                let resolved: Vec<i64> = prepared
                    .query_map(params.as_slice(), |r| r.get(0))?
                    .collect::<Result<_, _>>()?;
                resolved
            };

            if prune {
                let (n, bytes) = cache.prune(&cat, &ids)?;
                println!("pruned {n} stale entr(ies), {:.0} MB", bytes as f64 / 1e6);
                return Ok(());
            }

            let started = std::time::Instant::now();
            let on_progress = |done: usize, total: usize| {
                print!("\rthumbnails: {done}/{total}");
                let _ = std::io::Write::flush(&mut std::io::stdout());
            };
            let (made, skipped, failed) =
                npc::thumbs::prewarm(&cat, &cache, &ids, size, Some(&on_progress));
            println!();
            let secs = started.elapsed().as_secs_f64();

            println!("thumbnails ({} px) for {} stack(s)", size.px(), ids.len());
            println!("  generated  {made}");
            println!("  cached     {skipped}");
            if failed > 0 {
                println!("  failed     {failed}");
            }
            println!("  cache      {}", cache.dir().display());
            if made > 0 {
                println!(
                    "  took       {secs:.1}s  ({:.0}/s)",
                    made as f64 / secs.max(1e-9)
                );
            }
        }

        Cmd::Term => {
            println!(
                "TERM              {}",
                std::env::var("TERM").unwrap_or_default()
            );
            println!(
                "TERM_PROGRAM      {}",
                std::env::var("TERM_PROGRAM").unwrap_or_default()
            );
            println!(
                "KITTY_WINDOW_ID   {}",
                std::env::var("KITTY_WINDOW_ID").unwrap_or_default()
            );
            println!(
                "WEZTERM_PANE      {}",
                std::env::var("WEZTERM_PANE").unwrap_or_default()
            );
            println!();

            match ratatui_image::picker::Picker::from_query_stdio() {
                Ok(picker) => {
                    let (cw, ch) = picker.font_size();
                    println!("graphics protocol {:?}", picker.protocol_type());
                    println!("cell size         {cw}x{ch} px");
                    if let Ok((cols, rows)) = crossterm::terminal::size() {
                        println!(
                            "terminal          {cols}x{rows} cells = {}x{} px",
                            u32::from(cols) * u32::from(cw),
                            u32::from(rows) * u32::from(ch)
                        );
                        println!();
                        println!(
                            "a 5-across grid gives tiles of about {}x{} px",
                            (u32::from(cols) / 5) * u32::from(cw),
                            (u32::from(rows).saturating_sub(3) / 3) * u32::from(ch)
                        );
                    }
                    match picker.protocol_type() {
                        ratatui_image::picker::ProtocolType::Halfblocks => {
                            println!();
                            println!("Halfblocks is the fallback: one pixel per half character cell.");
                            println!("Photographs are not identifiable at that density. If this says");
                            println!("Halfblocks inside kitty, the graphics query failed rather than");
                            println!("kitty lacking support.");
                        }
                        p => println!("\n{p:?} is a real graphics protocol; thumbnails will be actual pixels."),
                    }
                }
                Err(e) => {
                    println!("could not query the terminal: {e}");
                    println!("(this needs a real tty -- not a pipe or a pager)");
                }
            }
        }

        Cmd::Export {
            query,
            out,
            root,
            size,
            quality,
            preset,
            keep_structure,
            strip,
            dry_run,
            list_presets,
        } => {
            let available = npc::export::presets();
            if list_presets {
                println!(
                    "built-in: {}px long edge, quality {}, from the largest",
                    npc::export::DEFAULT_LONG_EDGE,
                    npc::export::DEFAULT_QUALITY
                );
                println!("          embedded preview (full sensor resolution on a D750)");
                if available.is_empty() {
                    println!("\nno presets in {}", npc::config::Config::path().display());
                    println!("add one like:\n");
                    println!("  [[export]]");
                    println!("  name = \"web\"");
                    println!("  cmd  = [\"convert\", \"{{src}}\", \"-resize\", \"2000x2000>\", \"-quality\", \"95\", \"{{dst}}\"]");
                } else {
                    println!();
                    for p in &available {
                        println!("  {:<12} .{:<4} {}", p.name, p.ext, p.cmd.join(" "));
                    }
                }
                return Ok(());
            }

            let chosen = match &preset {
                Some(want) => {
                    Some(available.iter().find(|p| &p.name == want).ok_or_else(|| {
                        anyhow::anyhow!("no export preset named `{want}`; try --list-presets")
                    })?)
                }
                None => None,
            };

            let config = npc::config::Config::load()?;
            let root = library(root.as_deref())?;
            let out = export_dir(out.as_deref())?;

            let cat = require_online(&root)?;
            let target = Target {
                query: query.clone(),
                paths: Vec::new(),
                root: Some(root.clone()),
                dry_run,
            };
            let ids = resolve(&cat, &target)?;
            if ids.is_empty() {
                println!("nothing matched");
                return Ok(());
            }

            let opts = npc::export::Options {
                long_edge: size,
                quality,
                keep_structure,
                strip,
                preset: chosen,
                dry_run,
            };
            let started = std::time::Instant::now();
            let stats = npc::export::export(&cat, &ids, &out, &root, &config, &opts)?;
            let secs = started.elapsed().as_secs_f64();

            println!(
                "{} {} stack(s) -> {}",
                if dry_run { "would export" } else { "exported" },
                stats.written,
                out.display()
            );
            match chosen {
                Some(p) => println!("  preset    {}", p.name),
                None => println!("  {size}px long edge, quality {quality}"),
            }
            if stats.skipped > 0 {
                println!("  skipped   {} (no image member)", stats.skipped);
            }
            for e in stats.errors.iter().take(10) {
                eprintln!("  error: {e}");
            }
            if !dry_run && stats.written > 0 {
                println!("  took      {secs:.1}s");
            }
        }

        Cmd::Completion { shell } => {
            use clap::CommandFactory;
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }

        Cmd::Tui {
            root,
            all,
            trash,
            no_watch,
        } => {
            let trash_view = match (all, trash) {
                (_, true) => npc::tui::TrashView::Only,
                (true, false) => npc::tui::TrashView::Include,
                (false, false) => npc::tui::TrashView::Exclude,
            };
            npc::tui::run(library(root.get().as_deref())?, trash_view, no_watch)?;
        }

        Cmd::Status { root } => {
            let root = library(root.get().as_deref())?;

            let (cat, online) = open_checked(&root)?;
            println!("catalog {}", cat.path.display());
            let recorded: Option<String> = cat
                .conn
                .query_row("SELECT vol_uuid FROM volume LIMIT 1", [], |r| r.get(0))
                .ok();
            println!("  volume  {}", recorded.unwrap_or_else(|| "-".into()));
            println!(
                "  library {}",
                if online { "mounted" } else { "NOT MOUNTED" }
            );
            println!("  stacks  {}", cat.stack_count()?);
            println!("  files   {}", cat.file_count()?);
            println!("  orphans {}", cat.orphan_count()?);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insert_stack(cat: &Catalog, dir: &str, base: &str) {
        cat.conn
            .execute(
                "INSERT INTO stack (dir, base) VALUES (?1, ?2)",
                rusqlite::params![dir, base],
            )
            .unwrap();
    }

    /// The exact scenario `import`'s background thumbnail warm-up relies on:
    /// given the directories a run actually copied into, resolve precisely
    /// the stacks now living there -- no more (a same-day directory that
    /// merely looks similar), no fewer (every stack in a touched dir, not
    /// just the first).
    #[test]
    fn stack_ids_in_dirs_resolves_exactly_the_touched_directories() {
        let store = tempfile::tempdir().unwrap();
        let cat = Catalog::open_at(store.path().join("test.sqlite")).unwrap();
        insert_stack(&cat, "/lib/2026/07/23", "a");
        insert_stack(&cat, "/lib/2026/07/23", "b");
        insert_stack(&cat, "/lib/2026/07/24", "c");
        // A same-day directory from a *different* year -- must not be
        // pulled in by a substring-style match on "2026/07/23".
        insert_stack(&cat, "/lib/2020/07/23", "d");

        let mut dirs = std::collections::HashSet::new();
        dirs.insert(PathBuf::from("/lib/2026/07/23"));
        let mut ids = stack_ids_in_dirs(&cat, &dirs).unwrap();
        ids.sort_unstable();

        let mut expected: Vec<i64> = [
            cat.stack_id("/lib/2026/07/23", "a").unwrap().unwrap(),
            cat.stack_id("/lib/2026/07/23", "b").unwrap().unwrap(),
        ]
        .to_vec();
        expected.sort_unstable();
        assert_eq!(ids, expected);
    }

    #[test]
    fn stack_ids_in_dirs_is_empty_for_no_directories() {
        let store = tempfile::tempdir().unwrap();
        let cat = Catalog::open_at(store.path().join("test.sqlite")).unwrap();
        insert_stack(&cat, "/lib/2026/07/23", "a");

        let ids = stack_ids_in_dirs(&cat, &std::collections::HashSet::new()).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn progress_bar_fills_proportionally_and_never_overflows_its_width() {
        assert_eq!(progress_bar(0.0, 10), "[----------]");
        assert_eq!(progress_bar(1.0, 10), "[##########]");
        assert_eq!(progress_bar(0.5, 10), "[#####-----]");
        // A size total that undercounts by a few bytes (container overhead)
        // must not print a bar wider than requested.
        assert_eq!(progress_bar(1.2, 10), "[##########]");
        assert_eq!(progress_bar(-0.1, 10), "[----------]");
    }

    #[test]
    fn only_dir_accepts_a_real_subdirectory_of_the_library() {
        let lib = tempfile::tempdir().unwrap();
        let sub = lib.path().join("export/4claudia");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(only_dir_is_inside_library(lib.path(), &sub).unwrap(), sub);
    }

    /// `--only` hands the scan the dir spelled under the library root, so a
    /// detour through `..` stores the same dirs a full scan would.
    #[test]
    fn only_dir_is_spelled_under_the_library_root() {
        let lib = tempfile::tempdir().unwrap();
        let sub = lib.path().join("export/4cdsh");
        std::fs::create_dir_all(&sub).unwrap();
        let detour = lib.path().join("export/../export/4cdsh");
        assert_eq!(
            only_dir_is_inside_library(lib.path(), &detour).unwrap(),
            sub
        );
    }

    #[test]
    fn only_dir_refuses_a_sibling_directory_outside_the_library() {
        let lib = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let err = only_dir_is_inside_library(lib.path(), outside.path()).unwrap_err();
        assert!(err.to_string().contains("not inside the library"));
    }

    /// `..` inside an otherwise-plausible-looking path must not escape the
    /// containment check -- the whole reason this canonicalizes both sides
    /// first rather than comparing the strings as given.
    #[test]
    fn only_dir_refuses_a_dotdot_escape() {
        let lib = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(lib.path().join("export")).unwrap();
        // `..` all the way up to filesystem root from `lib/export`, then back
        // down the absolute path to `outside` -- lands exactly on `outside`
        // once canonicalized, by construction, whatever depth either sits at.
        let mut escape = lib.path().join("export");
        let depth = escape
            .components()
            .filter(|c| matches!(c, std::path::Component::Normal(_)))
            .count();
        for _ in 0..depth {
            escape = escape.join("..");
        }
        escape = escape.join(outside.path().strip_prefix("/").unwrap());

        let err = only_dir_is_inside_library(lib.path(), &escape).unwrap_err();
        assert!(err.to_string().contains("not inside the library"));
    }
}
