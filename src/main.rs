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
