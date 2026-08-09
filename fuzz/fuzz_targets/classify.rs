#![no_main]
//! Filenames come off a drive and can be anything a filesystem allows.
//! Classification must be total: an answer or None, never a panic.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(name) = std::str::from_utf8(data) else { return };
    if let Some(c) = npc::stack::classify(name) {
        // A base longer than the name it came from would mean the derived
        // suffix logic invented characters.
        assert!(c.base.len() <= name.len());
    }
});
