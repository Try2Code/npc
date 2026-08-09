//! Writes a single sidecar through the real code path, so external readers
//! (exiftool, digiKam) can be pointed at the result. Interop check, not a test.
fn main() {
    let path = std::env::args().nth(1).expect("usage: write_one <out.xmp>");
    let doc = npc::meta::SidecarDoc {
        exists: true,
        tags: vec!["beach".into(), "anna".into()],
        rating: 4,
        label: "Flagged".into(),
        title: "Ostsee, morning".into(),
        description: "first light, low tide".into(),
        orientation: 8,
    };
    npc::meta::write_sidecar(&path, &doc).unwrap();
    println!("wrote {path}");
}
