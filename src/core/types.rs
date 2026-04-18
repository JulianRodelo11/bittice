use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    In,
    Between,
    NotIn,
    IsNull,
    IsNotNull,
    Contains,
    StartsWith,
    EndsWith,
}

impl ComparisonOp {
    pub fn as_str(&self) -> &str {
        match self {
            ComparisonOp::Eq => "Eq",
            ComparisonOp::Ne => "Ne",
            ComparisonOp::Gt => "Gt",
            ComparisonOp::Gte => "Gte",
            ComparisonOp::Lt => "Lt",
            ComparisonOp::Lte => "Lte",
            ComparisonOp::In => "In",
            ComparisonOp::Between => "Between",
            ComparisonOp::NotIn => "NotIn",
            ComparisonOp::IsNull => "IsNull",
            ComparisonOp::IsNotNull => "IsNotNull",
            ComparisonOp::Contains => "Contains",
            ComparisonOp::StartsWith => "StartsWith",
            ComparisonOp::EndsWith => "EndsWith",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "Eq" => ComparisonOp::Eq,
            "Ne" => ComparisonOp::Ne,
            "Gt" => ComparisonOp::Gt,
            "Gte" => ComparisonOp::Gte,
            "Lt" => ComparisonOp::Lt,
            "Lte" => ComparisonOp::Lte,
            "In" => ComparisonOp::In,
            "Between" => ComparisonOp::Between,
            "NotIn" => ComparisonOp::NotIn,
            "IsNull" => ComparisonOp::IsNull,
            "IsNotNull" => ComparisonOp::IsNotNull,
            "Contains" => ComparisonOp::Contains,
            "StartsWith" => ComparisonOp::StartsWith,
            "EndsWith" => ComparisonOp::EndsWith,
            _ => ComparisonOp::Eq,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LogicalOp {
    And,
    Or,
}

impl std::fmt::Display for LogicalOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogicalOp::And => write!(f, "And"),
            LogicalOp::Or => write!(f, "Or"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    pub fn as_str(&self) -> &str {
        match self {
            SortDirection::Asc => "Asc",
            SortDirection::Desc => "Desc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum FieldType {
    String,
    Int,
    Float,
    Date,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: ComparisonOp,
    pub value: String,
    pub value_to: Option<String>,
    pub field_type: Option<FieldType>,
    #[serde(skip)]
    pub value_options: Vec<String>,
}

pub fn compare_values(left: &str, right: &str, field_type: Option<FieldType>) -> Option<Ordering> {
    match field_type {
        Some(FieldType::Int) | Some(FieldType::Float) => compare_numeric(left, right),
        Some(FieldType::Date) => compare_dates(left, right),
        _ => compare_numeric(left, right)
            .or_else(|| compare_dates(left, right))
            .or_else(|| Some(left.cmp(right))),
    }
}

pub fn compare_filter_value(
    actual: &str,
    op: ComparisonOp,
    value: &str,
    value_to: Option<&str>,
    value_options: &[String],
    field_type: Option<FieldType>,
) -> bool {
    match op {
        ComparisonOp::Eq => compare_values(actual, value, field_type) == Some(Ordering::Equal),
        ComparisonOp::Ne => compare_values(actual, value, field_type) != Some(Ordering::Equal),
        ComparisonOp::Gt => compare_values(actual, value, field_type) == Some(Ordering::Greater),
        ComparisonOp::Gte => matches!(compare_values(actual, value, field_type), Some(Ordering::Greater) | Some(Ordering::Equal)),
        ComparisonOp::Lt => compare_values(actual, value, field_type) == Some(Ordering::Less),
        ComparisonOp::Lte => matches!(compare_values(actual, value, field_type), Some(Ordering::Less) | Some(Ordering::Equal)),
        ComparisonOp::In => list_values(value, value_options)
            .iter()
            .any(|candidate| compare_values(actual, candidate, field_type) == Some(Ordering::Equal)),
        ComparisonOp::NotIn => list_values(value, value_options)
            .iter()
            .all(|candidate| compare_values(actual, candidate, field_type) != Some(Ordering::Equal)),
        ComparisonOp::Between => {
            let Some(upper) = value_to else {
                return false;
            };
            matches!(compare_values(actual, value, field_type), Some(Ordering::Greater) | Some(Ordering::Equal))
                && matches!(compare_values(actual, upper, field_type), Some(Ordering::Less) | Some(Ordering::Equal))
        }
        ComparisonOp::IsNull => is_nullish(actual),
        ComparisonOp::IsNotNull => !is_nullish(actual),
        ComparisonOp::Contains => actual.to_lowercase().contains(&value.to_lowercase()),
        ComparisonOp::StartsWith => actual.to_lowercase().starts_with(&value.to_lowercase()),
        ComparisonOp::EndsWith => actual.to_lowercase().ends_with(&value.to_lowercase()),
    }
}

pub fn list_values(value: &str, value_options: &[String]) -> Vec<String> {
    if !value_options.is_empty() {
        return value_options.to_vec();
    }

    value
        .split([',', ';'])
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn is_nullish(value: &str) -> bool {
    value.trim().is_empty()
}

fn compare_numeric(left: &str, right: &str) -> Option<Ordering> {
    let left_num = left.parse::<f64>().ok()?;
    let right_num = right.parse::<f64>().ok()?;
    left_num.partial_cmp(&right_num)
}

fn compare_dates(left: &str, right: &str) -> Option<Ordering> {
    if crate::core::date_utils::is_date_format(left) && crate::core::date_utils::is_date_format(right) {
        Some(left.cmp(right))
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregationResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    pub summary: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// Optional row identifiers (segment_id, local_id) for lazy-fetching
    pub row_ids: Option<Vec<(u64, u32)>>,
    pub total_found: usize,
    pub execution_time_micros: u128,
    pub debug_info: Option<String>,
    pub aggregations: Option<Vec<AggregationResult>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderBy {
    pub field: String,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthContext {
    pub user_id: String,
    pub token: String,
    pub entity: String,
    pub filter_col: String, // The column to filter in the business table
}
