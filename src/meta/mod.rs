//! The Exiv2 boundary.
//!
//! Two things are enforced here rather than remembered:
//!
//! 1. **Serialisation.** Exiv2's XMP layer wraps the Adobe SDK and is not
//!    thread-safe. Because this module is the only way into Exiv2, a single
//!    `Mutex` makes exclusion structural. Concurrent sidecar parsing corrupts
//!    the SDK's shared state intermittently rather than crashing cleanly, so
//!    "we're careful" is not good enough.
//!
//! 2. **The write invariant.** `write_sidecar` is the only function in the
//!    program that writes, and the C++ side refuses any path not ending in
//!    `.xmp`. Nothing else calls `writeMetadata()`.

use std::sync::{Mutex, OnceLock};

#[cxx::bridge(namespace = "npc")]
pub mod ffi {
    /// EXIF read off an original. Missing values are zero / empty rather than
    /// errors: an unknown camera must degrade to null columns, not abort a walk.
    #[derive(Debug, Clone, Default)]
    pub struct FileMeta {
        pub ok: bool,
        pub capture_time: i64, // unix seconds, 0 = unknown
        pub camera: String,
        pub lens: String,
        pub iso: u32,
        pub fnumber: f64,
        pub exposure: f64,
        pub focal: f64,
        /// Exposure compensation in EV. Zero is a real value, so `has_bias`
        /// distinguishes "shot at 0 EV" from "camera recorded nothing".
        pub exposure_bias: f64,
        pub has_bias: bool,
        pub width: u32,
        pub height: u32,
        pub orientation: u16, // as the camera wrote it; never overwritten
        pub serial: String,
        pub shutter_count: u32,
        pub rating: i32, // in-camera rating, -1 = unset
        /// Keywords embedded directly in the original -- `Xmp.dc.subject` and
        /// `Iptc.Application2.Keywords`, unioned. Distinct from a sidecar's
        /// tags: this is what other applications (Shotwell, digiKam) wrote
        /// into the file itself before this tool ever touched it.
        pub tags: Vec<String>,
    }

    /// The contents of a `.xmp` sidecar that we own. Foreign properties are not
    /// represented here on purpose -- they are preserved by never being read
    /// into Rust in the first place.
    #[derive(Debug, Clone, Default)]
    pub struct SidecarDoc {
        pub exists: bool,
        pub tags: Vec<String>,
        pub rating: i32, // -1 = unset
        pub label: String,
        pub title: String,
        pub description: String,
        pub orientation: u16, // 0 = unset
    }

    /// One EXIF value for an exported copy, as text Exiv2 parses into the
    /// key's own type (`"28/10"` for a rational). Applied only where the
    /// original has no value of its own.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ExifPair {
        pub key: String,
        pub value: String,
    }

    #[derive(Debug, Clone, Default)]
    pub struct PreviewBlob {
        pub ok: bool,
        pub error: String,
        pub data: Vec<u8>,
        pub width: u32,
        pub height: u32,
        pub mime: String,
    }

    unsafe extern "C++" {
        include!("src/meta/shim.h");

        fn xmp_init();
        fn xmp_shutdown();

        fn read_meta(path: &str) -> Result<FileMeta>;
        fn read_sidecar(xmp_path: &str) -> Result<SidecarDoc>;
        fn extract_preview(path: &str, min_dim: u32) -> Result<PreviewBlob>;

        fn write_sidecar(xmp_path: &str, doc: &SidecarDoc) -> Result<()>;
        fn stamp_export(
            dest_path: &str,
            source_path: &str,
            doc: &SidecarDoc,
            fallback: &[ExifPair],
            xp_keywords: &[u8],
        ) -> Result<()>;
    }
}

pub use ffi::{ExifPair, FileMeta, PreviewBlob, SidecarDoc};

fn lock() -> &'static Mutex<()> {
    static EXIV2: OnceLock<Mutex<()>> = OnceLock::new();
    EXIV2.get_or_init(|| {
        ffi::xmp_init();
        Mutex::new(())
    })
}

/// Poisoning is not meaningful here: the guard protects a C++ library's global
/// state, not Rust data that a panic could leave half-written.
macro_rules! guarded {
    ($body:expr) => {{
        let _g = lock().lock().unwrap_or_else(|e| e.into_inner());
        $body
    }};
}

/// Read EXIF from an original. Opens read-only.
pub fn read_meta(path: &str) -> anyhow::Result<FileMeta> {
    guarded!(ffi::read_meta(path).map_err(|e| anyhow::anyhow!("{path}: {e}")))
}

/// Read a sidecar. A missing file is `Ok` with `exists == false`, not an error.
pub fn read_sidecar(xmp_path: &str) -> anyhow::Result<SidecarDoc> {
    guarded!(ffi::read_sidecar(xmp_path).map_err(|e| anyhow::anyhow!("{xmp_path}: {e}")))
}

/// Extract the smallest embedded preview at least `min_dim` on its long edge.
pub fn extract_preview(path: &str, min_dim: u32) -> anyhow::Result<PreviewBlob> {
    guarded!(ffi::extract_preview(path, min_dim).map_err(|e| anyhow::anyhow!("{path}: {e}")))
}

/// The only write to a path beside your photographs. Refuses anything not
/// ending in `.xmp`.
pub fn write_sidecar(xmp_path: &str, doc: &SidecarDoc) -> anyhow::Result<()> {
    guarded!(ffi::write_sidecar(xmp_path, doc).map_err(|e| anyhow::anyhow!("{xmp_path}: {e}")))
}

/// Stamp metadata onto an exported copy: `source_path`'s EXIF (opened
/// read-only), `fallback` where it has none, and `doc`'s tags as IPTC
/// Keywords, EXIF XPKeywords and XMP dc:subject. The caller must already have
/// proved the destination is outside every library and protected root -- see
/// `export::check_target`. This never touches an original, because the file it
/// writes to is one this tool created moments earlier.
pub fn stamp_export(
    dest_path: &str,
    source_path: &str,
    doc: &SidecarDoc,
    fallback: &[ExifPair],
) -> anyhow::Result<()> {
    let xp = xp_keywords(&doc.tags);
    guarded!(
        ffi::stamp_export(dest_path, source_path, doc, fallback, &xp)
            .map_err(|e| anyhow::anyhow!("{dest_path}: {e}"))
    )
}

/// Windows' `XPKeywords`: UTF-16LE, `;`-separated, NUL-terminated. Empty for
/// no tags, so the tag is left out rather than written blank.
pub fn xp_keywords(tags: &[String]) -> Vec<u8> {
    if tags.is_empty() {
        return Vec::new();
    }
    tags.join(";")
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTF-16LE, `;` between tags, one NUL at the end: what Windows reads.
    #[test]
    fn xp_keywords_are_utf16_and_nul_terminated() {
        assert!(xp_keywords(&[]).is_empty());
        let b = xp_keywords(&["a".to_string(), "Kö".to_string()]);
        assert_eq!(b, vec![b'a', 0, b';', 0, b'K', 0, 0xF6, 0, 0, 0]);
    }
}
