//! A terminal photo catalog built around one rule: originals are never written.
//!
//! See PLAN.md. The short version: your `.NEF` and `.JPG` files are read-only
//! inputs. Everything this tool produces lives in a separate `.xmp` sidecar,
//! and the SQLite catalog is a rebuildable cache that can be thrown away
//! without losing anything.

pub mod db;
pub mod meta;
pub mod query;
pub mod scan;
pub mod stack;
pub mod vol;
