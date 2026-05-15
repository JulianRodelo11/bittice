//! Binary format definitions for the v3 exact index file.
//!
//! # File layout
//!
//! ```text
//! Offset  Bytes  Field
//! ──────  ─────  ──────────────────────────────────────────────────────────
//! 0       4      magic = b"BTXI"
//! 4       1      version = 3
//! 5       3      reserved = [0, 0, 0]
//! 8       8      num_entries: u64, little-endian
//! 16      8      bitmaps_section_offset: u64, little-endian  (= 40 always)
//! 24      8      index_section_offset: u64, little-endian    (variable)
//! 32      8      reserved_for_future: u64 = 0
//! ──────  ─────  ── header = 40 bytes ──────────────────────────────────────
//! 40      var    BITMAPS SECTION: bincode-serialized Vec<(u64, RoaringBitmap)>
//!                entries concatenated without padding.
//! ?       28×N   INDEX SECTION: N entries of 28 bytes each, sorted by hash asc.
//! ```
//!
//! Each **index entry** (28 bytes):
//! - hash: u128, little-endian (16 bytes)
//! - bitmap_offset: u64, little-endian (8 bytes) — absolute offset from file start
//! - bitmap_length: u32, little-endian (4 bytes) — length in bytes of the serialized bitmap

use std::fmt;
use std::io::{self, Write};

pub const EXACT_IDX_MAGIC: [u8; 4] = *b"BTXI";
pub const EXACT_IDX_VERSION_V3: u8 = 3;
pub const HEADER_SIZE: usize = 40;
pub const INDEX_ENTRY_SIZE: usize = 28;

/// The 40-byte file header for v3 exact index files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3Header {
    pub version: u8,
    pub num_entries: u64,
    pub bitmaps_section_offset: u64,
    pub index_section_offset: u64,
}

/// A single 28-byte entry in the index section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    pub hash: u128,
    pub offset: u64,
    pub length: u32,
}

/// Errors that can occur when reading or writing the v3 format.
#[derive(Debug)]
pub enum FormatError {
    /// File is too short to contain a valid header or section.
    TooShort { expected: usize, actual: usize },
    /// Magic bytes do not match `BTXI`.
    BadMagic { found: [u8; 4] },
    /// Version byte is not 3.
    UnsupportedVersion { found: u8 },
    /// `num_entries` value would require an index section larger than the file.
    InvalidNumEntries { value: u64, file_size: u64 },
    /// Underlying I/O error.
    IoError(io::Error),
    /// Bincode serialization/deserialization error.
    BincodeError(bincode::Error),
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::TooShort { expected, actual } => {
                write!(f, "exact_index v3: file too short (expected >= {}, got {})", expected, actual)
            }
            FormatError::BadMagic { found } => {
                write!(
                    f,
                    "exact_index v3: bad magic bytes {:?} (expected b\"BTXI\")",
                    found
                )
            }
            FormatError::UnsupportedVersion { found } => {
                write!(
                    f,
                    "exact_index v3: unsupported version {} (this reader only supports v3)",
                    found
                )
            }
            FormatError::InvalidNumEntries { value, file_size } => {
                write!(
                    f,
                    "exact_index v3: num_entries={} is inconsistent with file_size={} \
                     (file appears truncated or corrupted)",
                    value, file_size
                )
            }
            FormatError::IoError(e) => write!(f, "exact_index v3: I/O error: {}", e),
            FormatError::BincodeError(e) => write!(f, "exact_index v3: bincode error: {}", e),
        }
    }
}

impl std::error::Error for FormatError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FormatError::IoError(e) => Some(e),
            FormatError::BincodeError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FormatError {
    fn from(e: io::Error) -> Self {
        FormatError::IoError(e)
    }
}

impl From<bincode::Error> for FormatError {
    fn from(e: bincode::Error) -> Self {
        FormatError::BincodeError(e)
    }
}

// Note: `impl From<FormatError> for anyhow::Error` is provided by anyhow's
// blanket `impl<E: Error + Send + Sync + 'static> From<E> for anyhow::Error`.
// FormatError implements Error, so callers can use `?` in anyhow::Result contexts.

impl V3Header {
    /// Parse a 40-byte header from the given byte slice.
    ///
    /// Validates:
    /// - magic == b"BTXI"
    /// - version == 3
    /// - num_entries × INDEX_ENTRY_SIZE doesn't overflow u64
    pub fn parse(bytes: &[u8]) -> Result<V3Header, FormatError> {
        if bytes.len() < HEADER_SIZE {
            return Err(FormatError::TooShort {
                expected: HEADER_SIZE,
                actual: bytes.len(),
            });
        }

        let mut magic = [0u8; 4];
        magic.copy_from_slice(&bytes[0..4]);
        if magic != EXACT_IDX_MAGIC {
            return Err(FormatError::BadMagic { found: magic });
        }

        let version = bytes[4];
        if version != EXACT_IDX_VERSION_V3 {
            return Err(FormatError::UnsupportedVersion { found: version });
        }

        // bytes[5..8] are reserved, skip.
        let num_entries = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
        let bitmaps_section_offset = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let index_section_offset = u64::from_le_bytes(bytes[24..32].try_into().unwrap());
        // bytes[32..40] are reserved_for_future.

        // Validate that num_entries × INDEX_ENTRY_SIZE doesn't overflow.
        num_entries
            .checked_mul(INDEX_ENTRY_SIZE as u64)
            .ok_or(FormatError::InvalidNumEntries {
                value: num_entries,
                file_size: 0, // will be refined by caller
            })?;

        Ok(V3Header {
            version,
            num_entries,
            bitmaps_section_offset,
            index_section_offset,
        })
    }

    /// Write exactly 40 bytes to `out`:
    /// magic (4) + version (1) + reserved (3) + num_entries u64 LE (8) +
    /// bitmaps_section_offset u64 LE (8) + index_section_offset u64 LE (8) +
    /// reserved_for_future u64 LE (8)
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&EXACT_IDX_MAGIC)?;
        out.write_all(&[self.version, 0, 0, 0])?;
        out.write_all(&self.num_entries.to_le_bytes())?;
        out.write_all(&self.bitmaps_section_offset.to_le_bytes())?;
        out.write_all(&self.index_section_offset.to_le_bytes())?;
        out.write_all(&0u64.to_le_bytes())?; // reserved_for_future
        Ok(())
    }
}

impl IndexEntry {
    /// Parse a 28-byte index entry.
    /// Layout: hash u128 LE (16) + offset u64 LE (8) + length u32 LE (4)
    pub fn parse(bytes: &[u8; INDEX_ENTRY_SIZE]) -> IndexEntry {
        let hash = u128::from_le_bytes(bytes[0..16].try_into().unwrap());
        let offset = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let length = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        IndexEntry { hash, offset, length }
    }

    /// Write this index entry as exactly 28 bytes.
    pub fn write(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.hash.to_le_bytes())?;
        out.write_all(&self.offset.to_le_bytes())?;
        out.write_all(&self.length.to_le_bytes())?;
        Ok(())
    }
}
