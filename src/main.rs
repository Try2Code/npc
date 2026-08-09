use npc::meta;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "photo", about = "Terminal photo catalog", version)]
struct Cli {
    /// Fail any mutating operation at dispatch. On by default until the write
    /// path is verified; see PLAN.md.
    #[arg(long, global = true, default_value_t = true)]
    read_only: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Read and print everything we can learn about one file. Proves the bridge.
    Probe {
        path: String,
        /// Also list the embedded preview chosen for a given long-edge size
        #[arg(long)]
        preview: Option<u32>,
    },
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
    }

    Ok(())
}
