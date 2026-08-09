//! Generate one thumbnail and print where it landed, so a suspect image can
//! be inspected directly. Debugging aid, not part of the CLI.
fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .expect("usage: thumb_one <image> [grid|view]");
    let size = match std::env::args().nth(2).as_deref() {
        Some("view") => npc::thumbs::Size::View,
        _ => npc::thumbs::Size::Grid,
    };

    let m = npc::meta::read_meta(&path)?;
    let src = npc::thumbs::Source {
        path: std::path::PathBuf::from(&path),
        embedded: path.to_lowercase().ends_with(".nef"),
        mtime: 0,
        orientation: m.orientation,
        stack_id: 0,
    };

    let cache = npc::thumbs::Cache::new("debug")?;
    let out = cache.path_for(&src, size);
    let _ = std::fs::remove_file(&out);
    let made = cache.make_now(&src, size)?;

    println!("source      {path}");
    println!("  orient    {}", m.orientation);
    println!("  dims      {}x{}", m.width, m.height);
    println!("  thumbnail {}", made.display());
    Ok(())
}
