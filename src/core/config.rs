use serde::Serialize;

#[derive(Serialize)]
pub struct FieldConfig {
    pub field_name: String,
    pub indexed: bool,
    pub columnar: bool,
    pub extract_date_day: bool,
}

#[derive(Serialize)]
pub struct Config {
    pub indexed_fields: Vec<FieldConfig>,
    pub columnar_fields: Vec<String>,
}

#[derive(Serialize)]
pub struct FieldMetadata {
    pub name: String,
    pub count: u64,
}

// Stats internas para el analizador
pub struct FieldStats {
    pub is_date: bool,
    pub has_time: bool,
}
