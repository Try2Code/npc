//! Thin entry point for the GUI. All the actual work is in `npc::gui`.

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "npcg", about = "npc, as a native window", version)]
struct Cli {
    /// Library root. Defaults to `library` in the config.
    root: Option<PathBuf>,
    /// Disable the background library watcher for this session (overrides
    /// `[watch] enabled` in config.toml)
    #[arg(long)]
    no_watch: bool,
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let root = match cli.root {
        Some(r) => r,
        None => npc::config::Config::load()
            .unwrap_or_default()
            .library_root()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no library given. Pass a root, or set one in {}:\n\n    \
                     library = \"~/Pictures\"",
                    npc::config::Config::path().display()
                )
            })?,
    };

    npc::gui::run(&root, cli.no_watch)
}
