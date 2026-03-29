use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use crate::core::types::{OrderBy, Filter, FieldType};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", content = "details")]
pub enum SavedOperation {
    #[serde(rename = "read")]
    Read(SavedQuery),
    #[serde(rename = "insert")]
    Insert(SavedInsert),
    #[serde(rename = "update")]
    Update(SavedUpdate),
    #[serde(rename = "delete")]
    Delete(SavedDelete),
    #[serde(rename = "batch")]
    Batch(SavedBatch),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedBatch {
    pub name: String,
    pub operations: Vec<String>, // Names of other saved operations
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedQuery {
    pub name: String,
    pub entity: String,
    pub table: String,
    #[serde(default)]
    pub filters: Vec<SavedFilter>,
    #[serde(default = "default_filters_op")]
    pub filters_op: String, // "And" or "Or"
    #[serde(default)]
    pub aggregations: Vec<serde_json::Value>,
    #[serde(default)]
    pub order_by: Vec<SavedOrderBy>,
    pub limit: Option<usize>,
    /// When set (e.g. "$limit"), limit is a variable to be asked at runtime
    #[serde(default)]
    pub limit_param: Option<String>,
    #[serde(default)]
    pub selected_fields: Vec<String>,
}

fn default_filters_op() -> String {
    "And".to_string()
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedInsert {
    pub name: String,
    pub entity: String,
    pub table: String,
    /// List of fields that are expected in the payload.
    /// If empty, allows any field valid in schema.
    pub expected_fields: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedUpdate {
    pub name: String,
    pub entity: String,
    pub table: String,
    pub filters: Vec<SavedFilter>, // Conditions to find the record(s) to update
    pub allowed_fields: Vec<String>, // Fields allowed to be modified
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedDelete {
    pub name: String,
    pub entity: String,
    pub table: String,
    pub filters: Vec<SavedFilter>, // Conditions to find the record(s) to delete
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedFilter {
    pub field: String,
    pub op: String,
    pub value: String,
    pub field_type: Option<FieldType>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedOrderBy {
    pub field: String,
    pub direction: String,
}

impl From<&Filter> for SavedFilter {
    fn from(f: &Filter) -> Self {
        SavedFilter {
            field: f.field.clone(),
            op: f.op.as_str().to_string(),
            value: f.value.clone(),
            field_type: f.field_type,
        }
    }
}

impl From<&OrderBy> for SavedOrderBy {
    fn from(o: &OrderBy) -> Self {
        SavedOrderBy {
            field: o.field.clone(),
            direction: o.direction.as_str().to_string(),
        }
    }
}

impl SavedOperation {
    pub fn name(&self) -> &str {
        match self {
            SavedOperation::Read(q) => &q.name,
            SavedOperation::Insert(i) => &i.name,
            SavedOperation::Update(u) => &u.name,
            SavedOperation::Delete(d) => &d.name,
            SavedOperation::Batch(b) => &b.name,
        }
    }
}

pub fn save_operations(ops: &[SavedOperation]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(ops)?;
    let path = Path::new("data").join(".bittice_ops.json");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)?;
    Ok(())
}

pub fn load_operations() -> anyhow::Result<Vec<SavedOperation>> {
    let new_path = Path::new("data").join(".bittice_ops.json");
    let old_path_root = Path::new(".bittice_ops.json");
    let very_old_path = Path::new(".bittice_queries.json");

    // 1. Check if we have the file in the new location (data/.bittice_ops.json)
    if new_path.exists() {
        let content = fs::read_to_string(new_path)?;
        let ops: Vec<SavedOperation> = serde_json::from_str(&content)?;
        return Ok(ops);
    }

    // 2. Migration: Check if we have the file in the OLD location (root/.bittice_ops.json)
    if old_path_root.exists() {
        let content = fs::read_to_string(old_path_root)?;
        // Validate format
        let ops: Vec<SavedOperation> = serde_json::from_str(&content)?;
        // Move to new location
        save_operations(&ops)?;
        // Remove old file
        let _ = fs::remove_file(old_path_root);
        return Ok(ops);
    }

    // 3. Migration (Legacy): Check for .bittice_queries.json (very old format)
    if very_old_path.exists() {
        let content = fs::read_to_string(very_old_path)?;
        if let Ok(_) = serde_json::from_str::<Vec<Vec<SavedFilter>>>(&content) {
            if let Ok(queries) = serde_json::from_str::<Vec<SavedQuery>>(&content) {
                let ops: Vec<SavedOperation> = queries.into_iter().map(SavedOperation::Read).collect();
                save_operations(&ops)?; 
                return Ok(ops);
            }
        }
    }

    Ok(Vec::new())
}
