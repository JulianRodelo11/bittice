use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ComparisonOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Like,
    In,
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
            ComparisonOp::Like => "Like",
            ComparisonOp::In => "In",
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
            "Like" => ComparisonOp::Like,
            "In" => ComparisonOp::In,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Filter {
    pub field: String,
    pub op: ComparisonOp,
    pub value: String,
    #[serde(skip)]
    pub value_options: Vec<String>,
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
