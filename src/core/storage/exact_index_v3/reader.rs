//! Reader for the v3 exact index format.
//!
//! The file is opened and memory-mapped once in [`SnapshotReader::open`].
//! No data is deserialized until [`get`] or [`iter_entries`] is called.
//!
//! # Safety
//!
//! `memmap2::Mmap` requires an `unsafe` block.  The safety invariant is that
//! the underlying file is not modified while the mmap is live.  The v3
//! writer always writes to a `.tmp` file and then does an atomic rename;
//! the old snapshot's `SnapshotReader` keeps its `Mmap` pointing at the
//! old file descriptor (not the path), so concurrent reads remain safe even
//! when a new snapshot is written.

use std::path::{Path, PathBuf};
use std::fs;
use std::sync::Arc;

use memmap2::{Mmap, MmapOptions};
use roaring::RoaringBitmap;

use super::format::{
    FormatError, IndexEntry, V3Header, HEADER_SIZE, INDEX_ENTRY_SIZE,
};

/// A memory-mapped reader for a v3 exact index snapshot.
///
/// `Clone` is cheap: cloning shares the same underlying `Arc<Mmap>`.
pub struct SnapshotReader {
    mmap: Arc<Mmap>,
    num_entries: u64,
    bitmaps_section_offset: u64,
    index_section_offset: u64,
    path: PathBuf,
}

impl SnapshotReader {
    /// Open and validate a v3 exact index file.
    ///
    /// Parses the 40-byte header and validates that:
    /// - magic == b"BTXI", version == 3
    /// - `index_section_offset + num_entries * INDEX_ENTRY_SIZE == file_size`
    pub fn open(path: &Path) -> Result<SnapshotReader, FormatError> {
        let file = fs::File::open(path).map_err(FormatError::IoError)?;
        let file_size = file.metadata().map_err(FormatError::IoError)?.len();

        if (file_size as usize) < HEADER_SIZE {
            return Err(FormatError::TooShort {
                expected: HEADER_SIZE,
                actual: file_size as usize,
            });
        }

        // SAFETY: We hold an open file descriptor to the file.  The file
        // contents are accessed read-only.  The writer uses atomic rename, so
        // the mmap always points to a complete, consistent snapshot.
        let mmap = unsafe {
            MmapOptions::new()
                .map(&file)
                .map_err(FormatError::IoError)?
        };

        let header = V3Header::parse(&mmap[..HEADER_SIZE])?;

        // Validate: index_section_offset + num_entries * INDEX_ENTRY_SIZE == file_size
        let index_entries_size = header
            .num_entries
            .checked_mul(INDEX_ENTRY_SIZE as u64)
            .ok_or(FormatError::InvalidNumEntries {
                value: header.num_entries,
                file_size,
            })?;

        let expected_file_size = header
            .index_section_offset
            .checked_add(index_entries_size)
            .ok_or(FormatError::InvalidNumEntries {
                value: header.num_entries,
                file_size,
            })?;

        if expected_file_size != file_size {
            return Err(FormatError::InvalidNumEntries {
                value: header.num_entries,
                file_size,
            });
        }

        Ok(SnapshotReader {
            mmap: Arc::new(mmap),
            num_entries: header.num_entries,
            bitmaps_section_offset: header.bitmaps_section_offset,
            index_section_offset: header.index_section_offset,
            path: path.to_path_buf(),
        })
    }

    /// Returns the number of entries in this snapshot.
    pub fn num_entries(&self) -> u64 {
        self.num_entries
    }

    /// Returns the path this snapshot was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up a field value hash in the index.
    ///
    /// Uses binary search over the (already sorted) index section.
    /// Returns the deserialized `Vec<(segment_id, RoaringBitmap)>` on hit,
    /// or `None` on miss.
    pub fn get(
        &self,
        hash: u128,
    ) -> Result<Option<Vec<(u64, RoaringBitmap)>>, FormatError> {
        match self.find_entry(hash)? {
            None => Ok(None),
            Some(entry) => {
                let start = entry.offset as usize;
                let end = start + entry.length as usize;
                if end > self.mmap.len() {
                    return Err(FormatError::TooShort {
                        expected: end,
                        actual: self.mmap.len(),
                    });
                }
                let bitmap_slice = &self.mmap[start..end];
                let bitmaps: Vec<(u64, RoaringBitmap)> =
                    bincode::deserialize(bitmap_slice).map_err(FormatError::BincodeError)?;
                Ok(Some(bitmaps))
            }
        }
    }

    /// Iterate over all entries in the snapshot, in ascending hash order.
    ///
    /// Reads the index section sequentially; each bitmap is fetched from the
    /// bitmaps section at the position recorded in the index entry.
    /// Since bitmaps are written in the same hash-sorted order during
    /// `write_exact_index_v3`, these reads are sequential and cache-friendly.
    pub fn iter_entries(
        &self,
    ) -> impl Iterator<Item = Result<(u128, Vec<(u64, RoaringBitmap)>), FormatError>> + '_ {
        let num = self.num_entries;
        (0..num).map(move |i| {
            let entry = self.read_index_entry(i)?;
            let start = entry.offset as usize;
            let end = start + entry.length as usize;
            if end > self.mmap.len() {
                return Err(FormatError::TooShort {
                    expected: end,
                    actual: self.mmap.len(),
                });
            }
            let bitmap_slice = &self.mmap[start..end];
            let bitmaps: Vec<(u64, RoaringBitmap)> =
                bincode::deserialize(bitmap_slice).map_err(FormatError::BincodeError)?;
            Ok((entry.hash, bitmaps))
        })
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Binary-search the index section for the given hash.
    fn find_entry(&self, hash: u128) -> Result<Option<IndexEntry>, FormatError> {
        if self.num_entries == 0 {
            return Ok(None);
        }

        let mut lo: u64 = 0;
        let mut hi: u64 = self.num_entries;

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let entry = self.read_index_entry(mid)?;
            match entry.hash.cmp(&hash) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(Some(entry)),
            }
        }

        Ok(None)
    }

    /// Read and parse the i-th index entry (0-indexed).
    ///
    /// # Panics
    ///
    /// Panics if `i >= num_entries` (this is a programmer error; bounds are
    /// validated in `open()`).
    fn read_index_entry(&self, i: u64) -> Result<IndexEntry, FormatError> {
        let start = (self.index_section_offset + i * INDEX_ENTRY_SIZE as u64) as usize;
        let end = start + INDEX_ENTRY_SIZE;

        if end > self.mmap.len() {
            return Err(FormatError::TooShort {
                expected: end,
                actual: self.mmap.len(),
            });
        }

        // SAFETY: bounds validated by `open()` and the check above.
        let entry_bytes: &[u8; INDEX_ENTRY_SIZE] = self.mmap[start..end]
            .try_into()
            .expect("slice is exactly INDEX_ENTRY_SIZE bytes");

        Ok(IndexEntry::parse(entry_bytes))
    }
}

// Allow cheap clones (shares the Arc<Mmap>).
impl Clone for SnapshotReader {
    fn clone(&self) -> Self {
        SnapshotReader {
            mmap: Arc::clone(&self.mmap),
            num_entries: self.num_entries,
            bitmaps_section_offset: self.bitmaps_section_offset,
            index_section_offset: self.index_section_offset,
            path: self.path.clone(),
        }
    }
}

impl std::fmt::Debug for SnapshotReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotReader")
            .field("path", &self.path)
            .field("num_entries", &self.num_entries)
            .field("bitmaps_section_offset", &self.bitmaps_section_offset)
            .field("index_section_offset", &self.index_section_offset)
            .finish()
    }
}
