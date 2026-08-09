//! The tests that hold the central promise: this program never writes to an
//! original image file, and never destroys metadata another application owns.
//!
//! These run first and they run always. If any of them fails, nothing else
//! about the tool matters.

use npc::meta::{self, SidecarDoc};
use std::fs;
use std::path::Path;

fn doc_with_tags(tags: &[&str]) -> SidecarDoc {
    SidecarDoc {
        exists: true,
        tags: tags.iter().map(|s| s.to_string()).collect(),
        rating: -1,
        label: String::new(),
        title: String::new(),
        description: String::new(),
        orientation: 0,
    }
}

/// The guard. Every one of these is a path this tool must refuse, because
/// writing to any of them would modify a photograph.
#[test]
fn refuses_to_write_anything_that_is_not_a_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let doc = doc_with_tags(&["beach"]);

    for name in [
        "shot.NEF",
        "shot.JPG",
        "shot.jpg",
        "shot.tif",
        "shot.xmp.NEF", // .xmp appears, but not at the end
        "shot",
        "shot.XMP", // case matters: Exiv2 keys the sidecar type off ".xmp"
    ] {
        let path = dir.path().join(name);
        fs::write(&path, b"original bytes").unwrap();
        let before = fs::read(&path).unwrap();

        let result = meta::write_sidecar(path.to_str().unwrap(), &doc);

        assert!(
            result.is_err(),
            "write_sidecar accepted {name}, which would modify an original"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before,
            "{name} was modified despite the write being refused"
        );
    }
}

/// The one path that is allowed.
#[test]
fn accepts_a_sidecar_and_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("750_6926.xmp");
    let p = path.to_str().unwrap();

    let mut doc = doc_with_tags(&["beach", "anna"]);
    doc.rating = 4;
    doc.title = "Ostsee, morning".into();
    doc.description = "first light, low tide".into();

    meta::write_sidecar(p, &doc).expect("writing a .xmp sidecar must succeed");

    let back = meta::read_sidecar(p).unwrap();
    assert!(back.exists);
    assert_eq!(back.rating, 4);
    assert_eq!(back.title, "Ostsee, morning");
    assert_eq!(back.description, "first light, low tide");

    let mut tags = back.tags.clone();
    tags.sort();
    assert_eq!(tags, vec!["anna".to_string(), "beach".to_string()]);
}

/// The test that matters most. darktable, Lightroom and RawTherapee all park
/// private properties in these files. Rewriting `dc:subject` must not cost the
/// user anything they created in another application -- that is data loss they
/// would not discover for months.
#[test]
fn preserves_foreign_properties_written_by_other_applications() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("750_6926.xmp");
    let p = path.to_str().unwrap();

    // A sidecar as darktable would leave it, plus a Lightroom crop and an
    // acdsee field, so the test covers more than one foreign namespace.
    fs::write(
        &path,
        r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:darktable="http://darktable.sf.net/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
    xmlns:dc="http://purl.org/dc/elements/1.1/"
    darktable:history_end="7"
    darktable:iop_order_version="2"
    crs:CropTop="0.1"
    crs:Exposure2012="+0.35">
   <dc:subject>
    <rdf:Bag>
     <rdf:li>old-tag</rdf:li>
    </rdf:Bag>
   </dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#,
    )
    .unwrap();

    meta::write_sidecar(p, &doc_with_tags(&["beach"])).unwrap();

    let after = fs::read_to_string(&path).unwrap();

    for (needle, owner) in [
        ("history_end", "darktable"),
        ("iop_order_version", "darktable"),
        ("CropTop", "Lightroom"),
        ("Exposure2012", "Lightroom"),
    ] {
        assert!(
            after.contains(needle),
            "{owner}'s property `{needle}` was destroyed by our write.\n\
             This is silent data loss. Sidecar after write:\n{after}"
        );
    }

    // And our own change did land, replacing rather than appending.
    let back = meta::read_sidecar(p).unwrap();
    assert_eq!(back.tags, vec!["beach".to_string()]);
}

/// A source-level audit, cheap enough to run every time. Exactly two calls to
/// `writeMetadata()` may exist, both in shim.cc:
///
///   write_sidecar  -- refuses any path not ending in `.xmp`
///   stamp_export   -- writes only to a copy this tool just created, under a
///                     target proved to be outside every library and protected
///                     root (see export::check_target)
///
/// Neither can reach an original. A third would need the same argument made
/// for it, which is what this count is for -- it fails until someone justifies
/// the addition rather than sliding it in.
#[test]
fn writemetadata_appears_only_inside_the_guarded_writers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut offenders = Vec::new();
    let mut total = 0usize;

    for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let text = match fs::read_to_string(entry.path()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for (i, line) in text.lines().enumerate() {
            // Skip comments -- the header documents the rule and would match.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            if line.contains("writeMetadata()") {
                total += 1;
                let is_shim = entry.path().ends_with("shim.cc");
                if !is_shim {
                    offenders.push(format!("{}:{}", entry.path().display(), i + 1));
                }
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "writeMetadata() called outside src/meta/shim.cc: {offenders:?}"
    );
    assert_eq!(
        total, 2,
        "expected exactly two writeMetadata() calls (write_sidecar and \
         stamp_export), found {total}. Every additional one is a path that \
         could reach an original file, and needs justifying here."
    );
}
