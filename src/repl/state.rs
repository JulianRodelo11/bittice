use ratatui::widgets::ListState;
use std::collections::HashMap;
use roaring::RoaringBitmap;

// Pasos para el proceso de carga
#[derive(PartialEq)]
pub enum LoadStep {
    InputPath,
    InputEntity,
    InputTable,
    Processing,
    Done,
}

// Paneles principales en el modo búsqueda
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum SearchCriteria {
    Entity,
    Table,
    Filters,
    FiltersOp,
    Aggregations,
    OrderBy,
    Limit,
    Fields,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ComparisonOp {
    Eq,
    Ne,
    In,
    Like,
    Gt,
    Gte,
    Lt,
    Lte,
}

impl ComparisonOp {
    pub fn from_str(s: &str) -> Self {
        match s {
            "Eq" => ComparisonOp::Eq,
            "Ne" => ComparisonOp::Ne,
            "In" => ComparisonOp::In,
            "Like" => ComparisonOp::Like,
            "Gt" => ComparisonOp::Gt,
            "Gte" => ComparisonOp::Gte,
            "Lt" => ComparisonOp::Lt,
            "Lte" => ComparisonOp::Lte,
            _ => ComparisonOp::Eq,
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            ComparisonOp::Eq => "Eq",
            ComparisonOp::Ne => "Ne",
            ComparisonOp::In => "In",
            ComparisonOp::Like => "Like",
            ComparisonOp::Gt => "Gt",
            ComparisonOp::Gte => "Gte",
            ComparisonOp::Lt => "Lt",
            ComparisonOp::Lte => "Lte",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub field: String,
    pub op: ComparisonOp,
    pub value: String,
    pub value_options: Vec<String>,
}

#[derive(PartialEq, Clone, Copy, Debug)]
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


// Sub-paneles o pasos dentro de la sección de Filtros
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum FilterStep {
    List, 
    Field,
    Op,
    Value,
}

#[derive(PartialEq, Clone, Debug)]
pub enum AggregationStep {
    Main,
}

#[derive(PartialEq, Clone, Copy, Debug)]
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

pub struct OrderBy {
    pub field: String,
    pub direction: SortDirection,
}

// Para manejar el foco en la UI de búsqueda
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum FocusPanel {
    Left,   // Entity, Table, Filters, Aggregations...
    Middle, // Lista de Filtros / Lista de Agregaciones
    Right,  // Pasos de Filtro (Field, Op, Value) / Pasos de Agregación
    Extra,  // Opciones de Field/Op/Value
    Bottom, // Input para el valor
}

pub struct App {
    // --- Estado General ---
    pub menu_items: Vec<&'static str>,
    pub menu_state: ListState,
    pub active_task: Option<&'static str>,

    // --- Tarea: Load ---
    pub input_buffer: String,
    pub load_step: LoadStep,
    pub ndjson_path: String,
    pub entity_name: String,
    pub table_name: String,
    pub suggestions: Vec<String>,
    pub suggestion_index: Option<usize>,

    // --- Tarea: Search ---
    pub search_criteria: SearchCriteria,
    pub focus_panel: FocusPanel,
    
    // Datos y selecciones
    pub search_entities: Vec<String>,
    pub search_tables: Vec<String>,
    pub selected_entity: Option<String>,
    pub selected_table: Option<String>,
    
    // Estados de las listas en los paneles
    pub left_panel_state: ListState,
    pub middle_panel_state: ListState,
    pub right_panel_state: ListState,
    pub extra_panel_state: ListState,

    // --- Sub-tarea: Filters ---
    pub filters: Vec<Filter>,
    pub filters_op: LogicalOp,
    pub filter_step: FilterStep,
    pub available_fields: Vec<String>,
    pub selected_field: Option<String>,
    pub selected_op: ComparisonOp,
    pub filter_value_input: String,
    pub filter_value_options: Vec<String>,
    pub selected_value: Option<String>,

    // --- Sub-tarea: Aggregations ---
    pub aggregations: Vec<serde_json::Value>,
    pub agg_step: AggregationStep,
    pub agg_type_options: Vec<String>,
    pub agg_op_options: Vec<String>,
    pub agg_value_options: Vec<String>,

    // --- Sub-tarea: Order By ---
    pub order_by: Vec<OrderBy>,

    // --- Sub-tarea: Limit ---
    pub limit: Option<usize>,

    // --- Sub-tarea: Fields ---
    pub selected_fields: Vec<String>,

    // --- Query Results ---
    pub search_results: Option<crate::core::query::QueryResult>,
    pub results_scroll: u16,
    pub results_scroll_x: u16,
    pub results_page: usize,
    pub is_loading: bool,
    pub last_rendered_content_height: u16,
    pub results_viewport_height: u16,

    // Cache para queries: Field -> {Value -> Bitmap}
    pub query_cache: HashMap<String, HashMap<String, RoaringBitmap>>,

    // --- Notificaciones ---
    pub status_message: Option<(String, bool)>, // (Mensaje, EsÉxito)
}

impl App {
    pub fn new() -> App {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));

        let mut left_panel_state = ListState::default();
        left_panel_state.select(Some(0));

        App {
            // General
            menu_items: vec!["Load Data", "Search Data", "Exit"],
            menu_state,
            active_task: None,
            // Load
            input_buffer: String::new(),
            load_step: LoadStep::InputPath,
            ndjson_path: String::new(),
            entity_name: String::new(),
            table_name: String::new(),
            suggestions: Vec::new(),
            suggestion_index: None,
            // Search
            search_criteria: SearchCriteria::Entity,
            focus_panel: FocusPanel::Left,
            search_entities: Vec::new(),
            search_tables: Vec::new(),
            selected_entity: None,
            selected_table: None,
            left_panel_state,
            middle_panel_state: ListState::default(),
            right_panel_state: ListState::default(),
            extra_panel_state: ListState::default(),
            // Filters
            filters: Vec::new(),
            filters_op: LogicalOp::And,
            filter_step: FilterStep::List,
            available_fields: Vec::new(),
            selected_field: None,
            selected_op: ComparisonOp::Eq,
            filter_value_input: String::new(),
            filter_value_options: vec!["Write value".to_string()],
            selected_value: None,
            // Aggregations
            aggregations: Vec::new(),
            agg_step: AggregationStep::Main,
            agg_type_options: vec![
                "TopN".to_string(), "GroupBy".to_string(), "Sum".to_string(), 
                "Avg".to_string(), "Min".to_string(), "Max".to_string(),
                "ConsecutiveBuckets".to_string(), "RetentionByBucket".to_string(),
                "InactiveSinceBucket".to_string()
            ],
            agg_op_options: vec![
                "Sum".to_string(), "Count".to_string(), "Avg".to_string(), 
                "Min".to_string(), "Max".to_string()
            ],
            agg_value_options: vec!["Write value".to_string()],
            // Order By
            order_by: Vec::new(),
            // Limit
            limit: Some(100),
            // Fields
            selected_fields: Vec::new(),
            // Results
            search_results: None,
            results_scroll: 0,
            results_scroll_x: 0,
            results_page: 1,
            is_loading: false,
            last_rendered_content_height: 0,
            results_viewport_height: 0,
            query_cache: HashMap::new(),
            status_message: None,
        }
    }

    pub fn menu_next(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => if i >= self.menu_items.len() - 1 { 0 } else { i + 1 },
            None => 0,
        };
        self.menu_state.select(Some(i));
    }

    pub fn menu_previous(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => if i == 0 { self.menu_items.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.menu_state.select(Some(i));
    }
    
    pub fn suggestion_next(&mut self) {
        if self.suggestions.is_empty() { return; }
        let i = match self.suggestion_index {
            Some(i) => if i >= self.suggestions.len() - 1 { 0 } else { i + 1 },
            None => 0,
        };
        self.suggestion_index = Some(i);
    }

    pub fn suggestion_previous(&mut self) {
        if self.suggestions.is_empty() { return; }
        let i = match self.suggestion_index {
            Some(i) => if i == 0 { self.suggestions.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.suggestion_index = Some(i);
    }
}