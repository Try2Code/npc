#![no_main]
//! Sidecars are XML written by other applications, some of them long ago and
//! some of them badly. Reading one must never panic or abort -- a corrupt
//! sidecar is one skipped stack, not a dead scan.
use libfuzzer_sys::fuzz_target;
use std::io::Write;

fuzz_target!(|data: &[u8]| {
    let Ok(mut f) = tempfile::Builder::new().suffix(".xmp").tempfile() else { return };
    if f.write_all(data).is_err() {
        return;
    }
    let _ = f.flush();
    let _ = npc::meta::read_sidecar(&f.path().to_string_lossy());
});
