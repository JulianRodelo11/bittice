use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    #[serde(default)]
    pub computed_fields: Vec<SavedBatchComputedField>,
    #[serde(default)]
    pub response_mode: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedBatchComputedField {
    pub name: String,
    #[serde(default)]
    pub inputs: std::collections::HashMap<String, String>,
    pub expression: String,
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
    #[serde(default)]
    pub filter_tree: Option<SavedFilterTreeNode>,
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
    /// Optional execution strategy/profile for runtime specialization.
    #[serde(default)]
    pub execution_profile: Option<SavedExecutionProfile>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum SavedExecutionProfile {
    Name(String),
    Split(SavedSplitExecutionProfile),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedSplitExecutionProfile {
    /// Strategy identifier. Current supported value: "split_enrichment".
    pub mode: String,
    /// Field used to correlate base rows with enrichment rows.
    pub key_field: String,
    /// Runtime param name where collected keys are injected for enrichment query.
    #[serde(default = "default_split_ids_param")]
    pub ids_param: String,
    /// Optional key field in enrichment rows (defaults to `key_field`).
    #[serde(default)]
    pub enrichment_key_field: Option<String>,
    /// Base query keeps only these join aliases.
    #[serde(default)]
    pub base_join_aliases: Vec<String>,
    /// If set, base query keeps only selected output aliases.
    #[serde(default)]
    pub base_select_aliases: Vec<String>,
    /// Full enrichment query definition (executed as a second internal read).
    pub enrichment_query: Box<SavedQuery>,
    /// Fields copied from enrichment row into base row.
    #[serde(default)]
    pub merge_fields: Vec<String>,
    /// Numeric additive merge rules: target = base(target) + enrichment(source).
    #[serde(default)]
    pub additive_fields: Vec<SavedSplitAdditiveField>,
    /// Optional default values inserted on base rows before enrichment merge.
    #[serde(default)]
    pub defaults: std::collections::HashMap<String, serde_json::Value>,
    /// Extra debug marker appended to response meta.debug_info.
    #[serde(default)]
    pub debug_label: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedSplitAdditiveField {
    pub target_field: String,
    pub source_field: String,
}

fn default_split_ids_param() -> String {
    "split_ids".to_string()
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
    #[serde(default)]
    pub children: Vec<SavedResponseGrouping>,
    #[serde(default)]
    pub limit_grouping: Option<usize>,
    #[serde(default)]
    pub offset_grouping: Option<usize>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedCollectAggregation {
    #[serde(default, alias = "field")]
    pub group_by: Option<String>,
    #[serde(default, alias = "fields")]
    pub group_by_fields: Vec<String>,
    #[serde(default = "default_group_items_as")]
    pub items_as: String,
    #[serde(default)]
    pub item_fields: Vec<String>,
    #[serde(default)]
    pub order_by: Vec<SavedOrderBy>,
    #[serde(default, alias = "include_group_field_in_items")]
    pub include_group_fields_in_items: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedJoin {
    #[serde(rename = "type", default = "default_join_type")]
    pub join_type: String,
    #[serde(default)]
    pub entity: Option<String>,
    pub table: String,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub on: Vec<SavedJoinCondition>,
    /// When set, matching joined rows are not merged into extra base rows; instead one scalar
    /// field `{alias}.{count_matches_as}` is set to the number of matches (Left: 0 if none).
    #[serde(default)]
    pub count_matches_as: Option<String>,
    /// Column on the joined table to sum across all matches (must differ from `sum_matches_as`).
    /// Use with `sum_matches_as`; compatible with `count_matches_as` on the same join.
    #[serde(default)]
    pub sum_matches_field: Option<String>,
    /// Synthetic field `{alias}.{sum_matches_as}` set to the sum of `sum_matches_field` (Left: 0 if none).
    #[serde(default)]
    pub sum_matches_as: Option<String>,
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
    #[serde(default)]
    pub field: String,
    #[serde(default)]
    pub expression: Option<String>,
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
    /// `bittice_api_key` (or `api_key`): Bearer / X-API-Key with `bk_live_…`, prefix lookup + Argon2.
    /// Omitted or `legacy`: exact match on `token_col` (JWT-style tokens still supported).
    #[serde(default)]
    pub scheme: Option<String>,
}

impl SavedAuthConfig {
    pub fn uses_bittice_api_key(&self) -> bool {
        if let Some(scheme) = self.scheme.as_deref() {
            return scheme.eq_ignore_ascii_case("bittice_api_key")
                || scheme.eq_ignore_ascii_case("api_key");
        }
        self.table.eq_ignore_ascii_case("api_keys")
            && self.token_col.eq_ignore_ascii_case("prefix")
    }

    /// Fast-path scheme: prefix lookup + SHA-256 compare, no Argon2.
    /// Suitable for high-entropy machine credentials (~140 bits in
    /// `bk_live_<24 random>`), where the KDF cost of Argon2 buys nothing
    /// brute-force-resistant that the entropy hasn't already.
    pub fn uses_api_key_sha256(&self) -> bool {
        matches!(self.scheme.as_deref(), Some(s) if s.eq_ignore_ascii_case("api_key_sha256"))
    }
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
    #[serde(default)]
    pub value_to: Option<String>,
    #[serde(default)]
    pub values: Vec<String>,
    pub field_type: Option<FieldType>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum SavedFilterTreeNode {
    Condition(SavedFilter),
    Group(SavedFilterGroup),
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SavedFilterGroup {
    #[serde(default = "default_filters_op")]
    pub op: String,
    #[serde(default)]
    pub filters: Vec<SavedFilterTreeNode>,
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
            value_to: f.value_to.clone(),
            values: f.value_options.clone(),
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

impl SavedCollectAggregation {
    pub fn from_aggregation(value: &serde_json::Value) -> Result<Option<Self>, serde_json::Error> {
        let Some(params) = value.as_object().and_then(|object| object.get("Collect")) else {
            return Ok(None);
        };

        serde_json::from_value(params.clone()).map(Some)
    }

    pub fn group_fields(&self) -> Vec<String> {
        if !self.group_by_fields.is_empty() {
            self.group_by_fields.clone()
        } else {
            self.group_by.clone().into_iter().collect()
        }
    }
}

pub fn save_operations(ops: &[SavedOperation]) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(ops)?;
    let path = crate::core::data_paths::bittice_ops_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json)?;
    Ok(())
}

pub fn load_operations() -> anyhow::Result<Vec<SavedOperation>> {
    load_operations_with_filter(None)
}

/// Keys `entity/table` matching [`crate::server::table_manager::TableManager`] for every table
/// referenced by saved HTTP operations (reads, writes, batch expansion, joins, auth tables,
/// split enrichment queries). Used to prefer keeping these mirror tables in RAM under pressure.
pub fn collect_ops_query_table_keys(ops: &[SavedOperation]) -> HashSet<String> {
    let mut out = HashSet::new();
    let by_name: HashMap<String, &SavedOperation> =
        ops.iter().map(|o| (o.name().to_string(), o)).collect();
    let mut batch_visited: HashSet<String> = HashSet::new();
    for op in ops {
        collect_op_tables(op, &by_name, &mut batch_visited, &mut out);
    }
    out
}

fn ops_table_key(entity: &str, table: &str) -> String {
    format!("{}/{}", entity, table)
}

fn collect_op_tables(
    op: &SavedOperation,
    by_name: &HashMap<String, &SavedOperation>,
    batch_visited: &mut HashSet<String>,
    out: &mut HashSet<String>,
) {
    match op {
        SavedOperation::Read(q) => collect_read_query_tables(q, out),
        SavedOperation::Insert(i) => {
            out.insert(ops_table_key(&i.entity, &i.table));
        }
        SavedOperation::Update(u) => {
            out.insert(ops_table_key(&u.entity, &u.table));
        }
        SavedOperation::Delete(d) => {
            out.insert(ops_table_key(&d.entity, &d.table));
        }
        SavedOperation::Batch(b) => {
            if !batch_visited.insert(b.name.clone()) {
                return;
            }
            for name in &b.operations {
                if let Some(next) = by_name.get(name) {
                    collect_op_tables(next, by_name, batch_visited, out);
                }
            }
        }
    }
}

fn collect_read_query_tables(q: &SavedQuery, out: &mut HashSet<String>) {
    out.insert(ops_table_key(&q.entity, &q.table));
    for j in &q.joins {
        let ent = j.entity.as_deref().unwrap_or(q.entity.as_str());
        out.insert(ops_table_key(ent, &j.table));
    }
    if let Some(auth) = &q.auth_config {
        if auth.enabled {
            out.insert(ops_table_key(&q.entity, &auth.table));
        }
    }
    if let Some(profile) = &q.execution_profile {
        if let SavedExecutionProfile::Split(s) = profile {
            collect_read_query_tables(&s.enrichment_query, out);
        }
    }
}

pub fn load_operations_with_filter(entity_filter: Option<String>) -> anyhow::Result<Vec<SavedOperation>> {
    let new_path = crate::core::data_paths::bittice_ops_path();
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
