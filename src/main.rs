use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use npc::{db::Catalog, meta, scan, vol};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "photo", about = "Terminal photo catalog", version)]
struct Cli {
    /// Fail any mutating operation at dispatch. Defaults on until the write
    /// path is verified end to end; see PLAN.md.
    #[arg(long, global = true, default_value_t = true)]
    read_only: bool,

    #[command(subcommand)]
    cmd: Cmd,
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
        root: PathBuf,
        /// Reparse every file rather than trusting size+mtime
        #[arg(long)]
        full: bool,
    },
    /// Report which volume a path lives on, and where its catalog would go.
    Volume { path: PathBuf },
    /// Catalog summary.
    Status { root: PathBuf },
    /// Select stacks with the query language. See `photo find --help-syntax`.
    Find {
        /// e.g. "beach and rating>=4", "year=2026 camera~D750 -private"
        #[arg(default_value = "")]
        query: String,
        /// Which library to search
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
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
    },
    /// List tags with counts.
    Tags {
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
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
    /// Mark stacks flagged (pick) or rejected. These are mutually exclusive.
    Mark {
        /// `flag`, `reject`, or `clear`
        state: String,
        #[command(flatten)]
        target: Target,
    },
    /// Set a title or description.
    Set {
        /// `title` or `description`
        field: String,
        value: String,
        #[command(flatten)]
        target: Target,
    },
    /// The browser.
    Tui {
        #[arg(default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
    },
    /// Write every pending edit out to its sidecar.
    Sync {
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
    },
    /// Copy photos from a card into a dated library. The source is never modified.
    Import {
        /// Where the photos are now (a card, an inbox directory)
        source: PathBuf,
        /// Library root; files land in <library>/YYYY/MM/DD/
        #[arg(long)]
        library: PathBuf,
        /// Show the plan without copying anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Report shots that appear more than once.
    Dupes {
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
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
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
    },
    /// What is currently in the trash.
    TrashList {
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
    },
    /// Permanently delete everything in the trash. The only real delete.
    TrashEmpty {
        #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
        root: PathBuf,
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
    #[arg(long, default_value = "/home/ram/src/tasks/test-data")]
    root: PathBuf,
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

fn open_for(root: &Path) -> Result<Catalog> {
    let v = vol::identify(root)?;
    Catalog::open_for(&v.uuid, root)
}

/// Apply changes, then immediately flush. Edits are useless sitting in the
/// catalog: the files on disk are the source of truth.
fn edit_and_flush(t: &Target, changes: Vec<npc::edit::Change>, verb: &str) -> Result<()> {
    let config = npc::config::Config::load()?;
    let mut cat = open_for(&t.root)?;
    let ids = resolve(&cat, t)?;

    if ids.is_empty() {
        println!("nothing matched");
        return Ok(());
    }
    if t.dry_run {
        println!("would {verb} {} stack(s)", ids.len());
        return Ok(());
    }

    npc::edit::apply(&mut cat, &ids, &changes, &config)?;
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
    let n = rating.unwrap_or(0).clamp(0, 5) as usize;
    format!("{}{}", "*".repeat(n), " ".repeat(5 - n))
}

fn main() -> Result<()> {
    let cli = Cli::parse();

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

        Cmd::Scan { root, full } => {
            let started = std::time::Instant::now();
            let (stats, cat) = scan::scan_root(&root, full)?;
            let elapsed = started.elapsed();

            println!("scanned {}", root.display());
            println!("  catalog     {}", cat.path.display());
            println!("  files seen  {}", stats.files_seen);
            println!("  stacks      {}", stats.stacks);
            println!("  parsed      {}", stats.parsed);
            println!("  unchanged   {}", stats.unchanged);
            println!("  orphans     {}", stats.orphans);
            if stats.vanished > 0 {
                println!("  vanished    {} (marked absent, tags kept)", stats.vanished);
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
            println!("  took        {:.2}s  ({rate:.0} files/s)", elapsed.as_secs_f64());
        }

        Cmd::Find {
            query,
            root,
            files,
            print0,
            limit,
            explain,
        } => {
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

            let cat = open_for(&root)?;
            let mut prepared = cat.conn.prepare(&stmt)?;
            let params: Vec<&dyn rusqlite::ToSql> = compiled
                .params
                .iter()
                .map(|p| p as &dyn rusqlite::ToSql)
                .collect();

            let rows: Vec<(i64, String, String, Option<i64>, i64, i64, Option<i64>)> = prepared
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
            edit_and_flush(&target, vec![change], &format!("tag {action}"))?;
        }

        Cmd::Rate { value, target } => {
            let rating = if value == "-" || value == "clear" {
                None
            } else {
                let n: i64 = value.parse().context("rating must be 0-5 or `-`")?;
                anyhow::ensure!((0..=5).contains(&n), "rating must be between 0 and 5");
                Some(n)
            };
            edit_and_flush(&target, vec![npc::edit::Change::SetRating(rating)], "rated")?;
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
            edit_and_flush(&target, changes, &format!("marked {state}"))?;
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
            edit_and_flush(&target, vec![change], &format!("set {field} on"))?;
        }

        Cmd::Sync { root } => {
            let config = npc::config::Config::load()?;
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
            library,
            dry_run,
        } => {
            let config = npc::config::Config::load()?;
            config.ensure_mutable(&library)?;
            // Importing into a library that does not exist yet is the normal
            // first run, and must not fail on volume resolution.
            std::fs::create_dir_all(&library)
                .with_context(|| format!("creating library {}", library.display()))?;
            let mut cat = open_for(&library)?;

            let plans = npc::import::plan(&cat, &source, &library)?;
            let dupes = plans.iter().filter(|p| p.duplicate_of.is_some()).count();
            let fresh = plans.len() - dupes;

            println!("import {} -> {}", source.display(), library.display());
            println!("  stacks found      {}", plans.len());
            println!("  already in library {dupes}");
            println!("  to copy           {fresh}");

            let mut by_day: std::collections::BTreeMap<String, usize> = Default::default();
            for p in plans.iter().filter(|p| p.duplicate_of.is_none()) {
                *by_day
                    .entry(
                        p.dest_dir
                            .strip_prefix(&library)
                            .unwrap_or(&p.dest_dir)
                            .to_string_lossy()
                            .to_string(),
                    )
                    .or_default() += 1;
            }
            for (day, n) in &by_day {
                println!("    {day:<14} {n} stack(s)");
            }

            let stats = npc::import::execute(&plans, &config, dry_run)?;
            if dry_run {
                println!(
                    "\ndry run: would copy {} files, {:.1} GB",
                    stats.files_copied,
                    stats.bytes_copied as f64 / 1e9
                );
            } else {
                println!(
                    "\ncopied {} stacks, {} files, {:.1} GB (source untouched)",
                    stats.stacks_copied,
                    stats.files_copied,
                    stats.bytes_copied as f64 / 1e9
                );
                if stats.duplicates_skipped > 0 {
                    println!("skipped {} duplicate(s)", stats.duplicates_skipped);
                }
                for e in &stats.errors {
                    eprintln!("error: {e}");
                }
                let (s, _) = scan::scan_root(&library, false)?;
                println!("catalog now holds {} stacks", s.stacks);
            }
        }

        Cmd::Dupes { root } => {
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
            let mut cat = open_for(&target.root)?;
            let ids = resolve(&cat, &target)?;
            if ids.is_empty() {
                println!("nothing matched");
                return Ok(());
            }
            if target.dry_run {
                println!("would trash {} stack(s)", ids.len());
                return Ok(());
            }
            let stats = npc::trash::trash(&mut cat, &target.root, &ids, &config)?;
            println!(
                "trashed {} stack(s), {} files -> {}/.trash/",
                stats.stacks,
                stats.files,
                target.root.display()
            );
            println!("restore with `photo untrash <base>`; nothing was deleted");
            for e in &stats.errors {
                eprintln!("error: {e}");
            }
        }

        Cmd::Untrash { base, root } => {
            let config = npc::config::Config::load()?;
            let mut cat = open_for(&root)?;
            let entry = npc::trash::list(&cat)?
                .into_iter()
                .find(|e| e.base == base)
                .ok_or_else(|| anyhow::anyhow!("`{base}` is not in the trash"))?;
            let n = npc::trash::restore(&mut cat, entry.stack_id, &config)?;
            println!("restored {base}: {n} files -> {}", entry.orig_dir);
        }

        Cmd::TrashList { root } => {
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
            let mut cat = open_for(&root)?;
            let entries = npc::trash::list(&cat)?;
            if entries.is_empty() {
                println!("trash is empty");
                return Ok(());
            }
            if !yes {
                println!(
                    "{} stack(s) would be permanently deleted:",
                    entries.len()
                );
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

        Cmd::Tui { root } => npc::tui::run(root)?,

        Cmd::Status { root } => {
            let v = vol::identify(&root)?;
            let cat = open_for(&root)?;
            println!("catalog {}", cat.path.display());
            println!("  volume  {}", v.uuid);
            println!("  stacks  {}", cat.stack_count()?);
            println!("  files   {}", cat.file_count()?);
            println!("  orphans {}", cat.orphan_count()?);
        }
    }

    Ok(())
}
