use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
pub struct FieldConfig {
    pub field_name: String,
    pub indexed: bool,
    pub columnar: bool,
    pub extract_date_day: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Config {
    pub indexed_fields: Vec<FieldConfig>,
    pub columnar_fields: Vec<String>,
}

#[derive(Serialize, Deserialize)]
pub struct FieldMetadata {
    pub name: String,
    pub count: u64,
}

// Internal stats for the analyzer
pub struct FieldStats {
    pub is_date: bool,
    pub has_time: bool,
}
