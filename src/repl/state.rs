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

pub use crate::core::types::{ComparisonOp, Filter, LogicalOp, SortDirection, OrderBy, QueryResult};

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

// Para manejar el foco en la UI de búsqueda
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum FocusPanel {
    Left,   // Entity, Table, Filters, Aggregations...
    Middle, // Lista de Filtros / Lista de Agregaciones
    Right,  // Pasos de Filtro (Field, Op, Value) / Pasos de Agregación
    Extra,  // Opciones de Field/Op/Value
    Bottom, // Input para el valor
}

// Foco para la UI del servidor
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum ServerFocus {
    Endpoints,
    Logs,
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
    pub search_results: Option<QueryResult>,
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

    // --- Saved Queries ---
    pub is_saving_query: bool,
    pub show_saved_queries: bool,
    pub is_loading_to_edit: bool,
    pub loaded_query_name: Option<String>,
    pub save_query_name_input: String,
    pub saved_queries: Vec<crate::core::saved_queries::SavedOperation>,
    pub saved_queries_state: ListState,

    // --- Server ---
    pub is_server_running: bool,
    pub server_logs: Vec<String>,
    pub server_log_receiver: Option<tokio::sync::mpsc::Receiver<String>>,
    pub server_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub server_focus: ServerFocus,
    pub endpoint_state: ListState,
    pub log_state: ListState,

    // --- Parameterized Queries ---
    pub variable_prompt_queue: Vec<String>,
    pub variable_values: std::collections::HashMap<String, String>,
    pub is_prompting_variable: bool,
    pub current_variable: String,
    pub variable_input: String,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> App {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));

        let mut left_panel_state = ListState::default();
        left_panel_state.select(Some(0));

        // Load saved queries
        let saved_queries = crate::core::saved_queries::load_operations().unwrap_or_default();

        App {
            // General
            menu_items: vec!["Load", "Read", "Local Server", "Exit"],
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
            filter_value_options: vec!["Write value".to_string(), "Variable (ask later)".to_string()],
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
            agg_value_options: vec!["Write value".to_string(), "Variable (ask later)".to_string()],
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

            // Saved Queries
            is_saving_query: false,
            show_saved_queries: false,
            is_loading_to_edit: false,
            loaded_query_name: None,
            save_query_name_input: String::new(),
            saved_queries,
            saved_queries_state: ListState::default(),

            // Server
            is_server_running: false,
            server_logs: Vec::new(),
            server_log_receiver: None,
            server_shutdown_tx: None,
            server_focus: ServerFocus::Endpoints,
            endpoint_state: ListState::default(),
            log_state: ListState::default(),

            // Parameterized Queries
            variable_prompt_queue: Vec::new(),
            variable_values: std::collections::HashMap::new(),
            is_prompting_variable: false,
            current_variable: String::new(),
            variable_input: String::new(),
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