use anyhow::Result;
use clap::{Parser, Subcommand};
use npc::{db::Catalog, meta, scan, vol};
use std::path::PathBuf;

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

            let v = vol::identify(&root)?;
            let cat = Catalog::open_for_volume(&v.uuid)?;
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
            let v = vol::identify(&root)?;
            let cat = Catalog::open_for_volume(&v.uuid)?;
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

        Cmd::Status { root } => {
            let v = vol::identify(&root)?;
            let cat = Catalog::open_for_volume(&v.uuid)?;
            println!("catalog {}", cat.path.display());
            println!("  volume  {}", v.uuid);
            println!("  stacks  {}", cat.stack_count()?);
            println!("  files   {}", cat.file_count()?);
            println!("  orphans {}", cat.orphan_count()?);
        }
    }

    Ok(())
}
