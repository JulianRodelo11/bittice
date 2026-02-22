use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use crate::core::types::{OrderBy, Filter};

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
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedQuery {
    pub name: String,
    pub entity: String,
    pub table: String,
    pub filters: Vec<SavedFilter>,
    pub filters_op: String, // "And" or "Or"
    pub aggregations: Vec<serde_json::Value>,
    pub order_by: Vec<SavedOrderBy>,
    pub limit: Option<usize>,
    /// When set (e.g. "$limit"), limit is a variable to be asked at runtime
    #[serde(default)]
    pub limit_param: Option<String>,
    pub selected_fields: Vec<String>,
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
        }
    }
}

pub fn save_operations(ops: &[SavedOperation]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(ops)?;
    fs::write(".bittice_ops.json", json)?;
    Ok(())
}

pub fn load_operations() -> anyhow::Result<Vec<SavedOperation>> {
    // Migration: Check if old .bittice_queries.json exists and no .bittice_ops.json
    let old_path = Path::new(".bittice_queries.json");
    let new_path = Path::new(".bittice_ops.json");

    if !new_path.exists() && old_path.exists() {
        // Migrate old queries to new Read operations
        let content = fs::read_to_string(old_path)?;
        if let Ok(queries) = serde_json::from_str::<Vec<SavedQuery>>(&content) {
            let ops: Vec<SavedOperation> = queries.into_iter().map(SavedOperation::Read).collect();
            save_operations(&ops)?; // Save to new format
            // Optional: fs::remove_file(old_path)?; 
            return Ok(ops);
        }
    }

    if !new_path.exists() {
        return Ok(Vec::new());
    }
    
    let content = fs::read_to_string(new_path)?;
    let ops: Vec<SavedOperation> = serde_json::from_str(&content)?;
    Ok(ops)
}
