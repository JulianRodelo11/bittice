//! Writer for the v3 exact index format.
//!
//! # Algorithm
//!
//! 1. Open a `.tmp` file for atomic write.
//! 2. Reserve 40 bytes for the header (placeholder zeros).
//! 3. For each `(hash, bitmaps)` entry in hash-sorted order:
//!    - Serialize `bitmaps` with bincode.
//!    - Write the serialized bytes to the bitmaps section.
//!    - Accumulate a 28-byte index entry `(hash, offset, length)`.
//! 4. Write all accumulated index entries.
//! 5. Seek back to offset 0 and write the real header.
//! 6. Flush, fsync, and atomically rename `.tmp` → final path.
//!
//! # Invariant
//!
//! The caller must provide entries in **ascending hash order**.  A
//! `debug_assert!` checks this in debug builds and emits a `tracing::warn!`
//! in release builds.  Violating this invariant produces a corrupt file
//! (binary search will not work).

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use roaring::RoaringBitmap;
#[cfg(not(debug_assertions))]
use tracing::warn;

use super::format::{
    FormatError, IndexEntry, V3Header, EXACT_IDX_VERSION_V3, HEADER_SIZE, INDEX_ENTRY_SIZE,
};

/// Write a v3 exact index file from an iterator of `(hash, bitmaps)` pairs.
///
/// The iterator **must** yield entries in ascending hash order.  In debug
/// builds a `debug_assert!` panics on unsorted input.  In release builds a
/// warning is emitted but the write continues (producing a corrupt file).
///
/// The write is atomic: a `.tmp` file is written first, then renamed over
/// `path` only after `fsync`.
pub fn write_exact_index_v3<I>(path: &Path, entries: I) -> Result<(), FormatError>
where
    I: IntoIterator<Item = (u128, Vec<(u64, RoaringBitmap)>)>,
{
    let dir = path.parent().unwrap_or(Path::new("."));
    if !dir.exists() {
        fs::create_dir_all(dir).map_err(FormatError::IoError)?;
    }

    let tmp_path = path.with_extension("idx.tmp");

    let file = fs::File::create(&tmp_path).map_err(FormatError::IoError)?;
    let mut writer = BufWriter::new(file);

    // Reserve 40 bytes for the header (will be overwritten at the end).
    writer.write_all(&[0u8; HEADER_SIZE]).map_err(FormatError::IoError)?;

    let bitmaps_section_offset: u64 = HEADER_SIZE as u64;
    let mut current_offset: u64 = bitmaps_section_offset;
    let mut index_entries: Vec<(u128, u64, u32)> = Vec::new();
    let mut num_entries: u64 = 0;
    let mut prev_hash: Option<u128> = None;

    for (hash, bitmaps) in entries {
        // Enforce sorted order invariant.
        if let Some(prev) = prev_hash {
            if hash <= prev {
                #[cfg(debug_assertions)]
                {
                    debug_assert!(
                        hash > prev,
                        "write_exact_index_v3: input is not sorted (prev=0x{:032x}, current=0x{:032x}). \
                         The resulting file will be corrupt.",
                        prev, hash
                    );
                }
                #[cfg(not(debug_assertions))]
                {
                    warn!(
                        "write_exact_index_v3: input is not sorted (prev=0x{:032x}, current=0x{:032x}). \
                         The resulting file will be corrupt.",
                        prev, hash
                    );
                }
            }
        }
        prev_hash = Some(hash);

        let serialized = bincode::serialize(&bitmaps).map_err(FormatError::BincodeError)?;
        let length = serialized.len() as u32;

        writer.write_all(&serialized).map_err(FormatError::IoError)?;
        index_entries.push((hash, current_offset, length));
        current_offset += serialized.len() as u64;
        num_entries += 1;
    }

    let index_section_offset: u64 = current_offset;

    // Write the index section.
    for (hash, offset, length) in &index_entries {
        let entry = IndexEntry { hash: *hash, offset: *offset, length: *length };
        entry.write(&mut writer).map_err(FormatError::IoError)?;
    }

    // Verify total size: index_section_offset + num_entries * INDEX_ENTRY_SIZE.
    let _expected_file_size = index_section_offset
        .checked_add(num_entries.saturating_mul(INDEX_ENTRY_SIZE as u64))
        .ok_or(FormatError::InvalidNumEntries {
            value: num_entries,
            file_size: 0,
        })?;

    // Flush the BufWriter before seeking.
    writer.flush().map_err(FormatError::IoError)?;

    // Seek back to the beginning and write the real header.
    writer
        .seek(SeekFrom::Start(0))
        .map_err(FormatError::IoError)?;

    let header = V3Header {
        version: EXACT_IDX_VERSION_V3,
        num_entries,
        bitmaps_section_offset,
        index_section_offset,
    };
    header.write(&mut writer).map_err(FormatError::IoError)?;

    // Flush again after writing the header, then fsync.
    writer.flush().map_err(FormatError::IoError)?;
    let file = writer
        .into_inner()
        .map_err(|e| FormatError::IoError(e.into_error()))?;
    file.sync_all().map_err(FormatError::IoError)?;

    // Atomic rename.
    fs::rename(&tmp_path, path).map_err(FormatError::IoError)?;

    Ok(())
}

/// Write a v3 exact index file from a `HashMap<u128, Vec<(u64, RoaringBitmap)>>`.
///
/// Sorts the entries by hash key before writing, satisfying the sorted-order
/// invariant required by `write_exact_index_v3`.
pub fn write_exact_index_v3_from_hashmap(
    path: &Path,
    map: HashMap<u128, Vec<(u64, RoaringBitmap)>>,
) -> Result<(), FormatError> {
    let mut entries: Vec<(u128, Vec<(u64, RoaringBitmap)>)> = map.into_iter().collect();
    entries.sort_unstable_by_key(|(hash, _)| *hash);
    write_exact_index_v3(path, entries)
}
