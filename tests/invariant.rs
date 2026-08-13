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
        tags: tags.iter().map(std::string::ToString::to_string).collect(),
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

    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(entry.path()) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            // Skip comments -- the header documents the rule and would match.
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with('*') {
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

/// A second source-level audit, in the same spirit as the count above: every
/// argument this tool advertises in `--help` must actually be consulted.
///
/// This exists because `--read-only` did not. It was declared global, defaulted
/// to `true`, and told every subcommand's `--help` that it would "fail any
/// mutating operation at dispatch" -- while nothing ever read it. Anyone
/// checking `npc scan --help` would conclude their originals were protected at
/// dispatch. They were not. The real guarantees are the shim's `.xmp` assertion
/// and `protected_roots`, both of which work; the flag was decoration over
/// them.
///
/// No clippy lint catches this, which is why it is a test. clap's derived
/// `Parser` impl both writes and reads the field, so `dead_code` never fires --
/// the deadness is behavioural, not lexical, and only a reader who asks "does
/// anything consult this?" can see it.
#[test]
fn every_declared_cli_flag_is_actually_read() {
    let main_rs = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    let text = fs::read_to_string(&main_rs).expect("reading src/main.rs");

    let code_lines = || {
        text.lines().filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with('*')
        })
    };

    // Field names carrying an `#[arg(...)]` attribute: exactly the flags and
    // positionals clap puts in front of the user.
    let mut declared: Vec<(String, usize)> = Vec::new();
    let mut pending = false;
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with('*') {
            continue;
        }
        if t.starts_with("#[arg") {
            pending = true;
            continue;
        }
        if t.starts_with('#') {
            continue; // some other attribute; the field is still coming
        }
        if pending {
            if let Some(name) = field_name(t) {
                declared.push((name, i + 1));
            }
            pending = false;
        }
    }

    assert!(
        !declared.is_empty(),
        "no #[arg] fields found at all -- this audit has stopped working, \
         which is worse than it failing"
    );

    // An identifier occurring once occurs only in its own declaration, so
    // nothing anywhere consults the parsed value.
    let dead: Vec<String> = declared
        .iter()
        .filter(|(name, _)| code_lines().map(|l| idents(l, name)).sum::<usize>() < 2)
        .map(|(name, line)| format!("{name} (src/main.rs:{line})"))
        .collect();

    assert!(
        dead.is_empty(),
        "these arguments are declared, and advertised in --help, but never \
         read: {dead:?}. Consult the value or remove the argument -- a flag \
         promising behaviour it does not deliver is worse than no flag."
    );
}

/// `name: Type,` -> `name`. `None` for anything that is not a field.
fn field_name(line: &str) -> Option<String> {
    let name = line.split_once(':')?.0.trim();
    let plausible = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    plausible.then(|| name.to_string())
}

/// Occurrences of `needle` in `line` as a whole identifier, so `root` does not
/// match inside `protected_roots`.
fn idents(line: &str, needle: &str) -> usize {
    line.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|tok| *tok == needle)
        .count()
}
