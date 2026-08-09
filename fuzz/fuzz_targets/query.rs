#![no_main]
//! The query parser takes arbitrary text from a person in a hurry. The
//! property worth fuzzing is totality: any input either compiles or returns an
//! error, and never panics.
//!
//! Two earlier attempts here asserted "no quote from the input reaches the
//! SQL" and both fired on the tool's own templates -- `COALESCE(x,'')` and
//! `date(t,'unixepoch')` contain legitimate quoted literals. Injection is a
//! property about *values*, and it is pinned deterministically by
//! `values_are_never_interpolated_into_sql` with a known-hostile string.
//! Fuzzing is the wrong instrument for it, and a fuzz target that cries wolf
//! is worse than none.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let Ok(compiled) = npc::query::compile(text) else { return };

    // Every bound value must have a placeholder referencing it. A mismatch
    // means the statement would fail at execution, far from the cause.
    for i in 1..=compiled.params.len() {
        assert!(
            compiled.where_sql.contains(&format!("?{i}")),
            "value {i} of {} is bound but never referenced:\n{}",
            compiled.params.len(),
            compiled.where_sql
        );
    }

    // And the compiled fragment must be something SQLite will accept. This is
    // what turns a parser bug into a test failure rather than a runtime error
    // in front of the user.
    if let Ok(cat) = npc::db::Catalog::open_memory() {
        let stmt = compiled.select_stacks(Some(1));
        assert!(
            cat.conn.prepare(&stmt).is_ok(),
            "generated SQL was rejected by SQLite:\n{stmt}"
        );
    }
});
