//! A terminal photo catalog built around one rule: originals are never written.
//!
//! See PLAN.md. The short version: your `.NEF` and `.JPG` files are read-only
//! inputs. Everything this tool produces lives in a separate `.xmp` sidecar,
//! and the SQLite catalog is a rebuildable cache that can be thrown away
//! without losing anything.

pub mod action;
pub mod cards;
pub mod config;
pub mod db;
pub mod doctor;
pub mod dupes;
pub mod edit;
pub mod exif;
pub mod export;
pub mod import;
pub mod meta;
pub mod metadata_merge;
pub mod metadata_sync;
pub mod query;
pub mod scan;
pub mod stack;
pub mod thumbs;
pub mod trash;
pub mod tui;
pub mod vol;
