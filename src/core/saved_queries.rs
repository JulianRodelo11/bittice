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
    pub table_alias: Option<String>,
    #[serde(default)]
    pub joins: Vec<SavedJoin>,
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
    #[serde(default)]
    pub select: Vec<SavedSelectField>,
    #[serde(default)]
    pub response_grouping: Option<SavedResponseGrouping>,
    /// Configuration for Row-Level Security via Bearer Token
    #[serde(default)]
    pub auth_config: Option<SavedAuthConfig>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedResponseGrouping {
    #[serde(default)]
    pub field: Option<String>,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default = "default_group_items_as")]
    pub items_as: String,
    #[serde(default, alias = "include_group_field_in_items")]
    pub include_group_fields_in_items: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedJoin {
    #[serde(rename = "type", default = "default_join_type")]
    pub join_type: String,
    pub table: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub on: Vec<SavedJoinCondition>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedJoinCondition {
    pub left: String,
    #[serde(default = "default_join_op")]
    pub op: String,
    pub right: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedSelectField {
    pub field: String,
    #[serde(rename = "as", default)]
    pub output_name: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedAuthConfig {
    pub enabled: bool,
    pub table: String,
    pub token_col: String,
    pub id_col: String,
    pub filter_col: String,
}

fn default_filters_op() -> String {
    "And".to_string()
}

fn default_join_type() -> String {
    "Inner".to_string()
}

fn default_join_op() -> String {
    "Eq".to_string()
}

fn default_group_items_as() -> String {
    "items".to_string()
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

impl SavedQuery {
    pub fn is_multi_table(&self) -> bool {
        !self.joins.is_empty()
    }

    pub fn base_alias(&self) -> String {
        self.table_alias
            .as_ref()
            .map(|alias| alias.trim())
            .filter(|alias| !alias.is_empty())
            .unwrap_or(self.table.as_str())
            .to_string()
    }
}

impl SavedResponseGrouping {
    pub fn group_fields(&self) -> Vec<String> {
        if !self.fields.is_empty() {
            self.fields.clone()
        } else {
            self.field.clone().into_iter().collect()
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
    load_operations_with_filter(None)
}

pub fn load_operations_with_filter(entity_filter: Option<String>) -> anyhow::Result<Vec<SavedOperation>> {
    let new_path = Path::new("data").join(".bittice_ops.json");
    let old_path_root = Path::new(".bittice_ops.json");
    let very_old_path = Path::new(".bittice_queries.json");

    let mut all_ops = Vec::new();

    // 1. Try new path
    if new_path.exists() {
        let content = fs::read_to_string(&new_path)?;
        all_ops = serde_json::from_str(&content).unwrap_or_default();
    } else if old_path_root.exists() {
        // 2. Migrate from old root path
        let content = fs::read_to_string(&old_path_root)?;
        all_ops = serde_json::from_str(&content).unwrap_or_default();
        let _ = save_operations(&all_ops);
        let _ = fs::remove_file(old_path_root);
    } else if very_old_path.exists() {
        // 3. Migrate from legacy .bittice_queries.json
        let content = fs::read_to_string(&very_old_path)?;
        if let Ok(queries) = serde_json::from_str::<Vec<SavedQuery>>(&content) {
            all_ops = queries.into_iter().map(SavedOperation::Read).collect();
            let _ = save_operations(&all_ops);
            // We don't remove very_old_path to be safe
        }
    }

    if let Some(filter) = entity_filter {
        let filter_lower = filter.to_lowercase();
        let filtered = all_ops.into_iter().filter(|op| {
            let entity = match op {
                SavedOperation::Read(q) => &q.entity,
                SavedOperation::Insert(i) => &i.entity,
                SavedOperation::Update(u) => &u.entity,
                SavedOperation::Delete(d) => &d.entity,
                SavedOperation::Batch(_) => "",
            };
            entity.is_empty() || entity.to_lowercase() == filter_lower
        }).collect();
        Ok(filtered)
    } else {
        Ok(all_ops)
    }
}
