use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SegmentMeta {
    pub id: u64,
    /// Minimum and Maximum values for each column in this segment.
    /// Used for query pruning (Data Skipping).
    /// Key: Column Name, Value: (Min, Max)
    pub min_max: HashMap<String, (String, String)>,
    pub created_at: i64,
    pub record_count: u64,
    /// Path relative to table root where segment data is stored
    pub path: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Manifest {
    /// Active (immutable) segments that make up the table state
    pub segments: Vec<SegmentMeta>,
    /// The ID of the currently active write segment (mutable)
    pub active_segment_id: u64,
    /// Monotonic sequence number for WAL consistency
    pub last_sequence_number: u64,
    /// Version of the manifest file itself
    pub version: u64,
    /// Fields present in the original source data
    #[serde(default)]
    pub original_fields: Vec<String>,
    /// Field name used as primary key (legacy: first column when composite).
    #[serde(default)]
    pub primary_key: String,
    /// Full MySQL PRIMARY KEY column list in ordinal order (composite keys).
    #[serde(default)]
    pub primary_key_columns: Vec<String>,
}

impl Manifest {
    pub fn new() -> Self {
        Manifest {
            segments: Vec::new(),
            active_segment_id: 0,
            last_sequence_number: 0,
            version: 1,
            original_fields: Vec::new(),
            primary_key: "PK".to_string(),
            primary_key_columns: Vec::new(),
        }
    }

    pub fn add_segment(&mut self, segment: SegmentMeta) {
        self.segments.push(segment);
    }

    pub fn remove_segment(&mut self, segment_id: u64) {
        self.segments.retain(|s| s.id != segment_id);
    }
}
