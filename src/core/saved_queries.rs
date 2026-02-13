use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use crate::repl::state::{OrderBy, Filter};

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
    pub selected_fields: Vec<String>,
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

pub fn save_queries(queries: &[SavedQuery]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(queries)?;
    fs::write(".bittice_queries.json", json)?;
    Ok(())
}

pub fn load_queries() -> anyhow::Result<Vec<SavedQuery>> {
    if !Path::new(".bittice_queries.json").exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(".bittice_queries.json")?;
    let queries: Vec<SavedQuery> = serde_json::from_str(&content)?;
    Ok(queries)
}
