//! Standalone v3 exact index format: writer, reader, and migration.
//!
//! This module implements the v3 binary format for exact index files as
//! described in `docs/exact_index_v3_format.md`.  It does NOT modify the
//! existing `ExactIndex` struct or its callers — it is a standalone
//! implementation used to validate the format end-to-end before integration
//! (Phase 1d-integrate).

pub mod format;
pub mod migrate;
pub mod reader;
pub mod writer;

pub use reader::SnapshotReader;
pub use writer::{write_exact_index_v3, write_exact_index_v3_from_hashmap};
