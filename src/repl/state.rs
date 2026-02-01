use ratatui::widgets::ListState;

// Pasos para el proceso de carga
pub enum LoadStep {
    InputPath,
    InputEntity,
    InputTable,
    Processing,
    Done,
}

pub struct App {
    // Estado del menú superior
    pub menu_items: Vec<&'static str>,
    pub menu_state: ListState,

    // Estado de la tarea activa
    pub active_task: Option<&'static str>, // None, Some("Load"), Some("Search")

    // Estado de Input para el contenedor de abajo
    pub input_buffer: String,
    pub load_step: LoadStep,
    pub ndjson_path: String,
    pub entity_name: String,
    pub table_name: String,
    pub processing_message: String,

    // Estado de autocompletado
    pub suggestions: Vec<String>,
    pub suggestion_index: Option<usize>,
}

impl App {
    pub fn new() -> App {
        let mut menu_state = ListState::default();
        menu_state.select(Some(0));
        App {
            menu_items: vec!["Load Data", "Search Data", "Exit"],
            menu_state,
            active_task: None,
            input_buffer: String::new(),
            load_step: LoadStep::InputPath,
            ndjson_path: String::new(),
            entity_name: String::new(),
            table_name: String::new(),
            processing_message: String::new(),
            suggestions: Vec::new(),
            suggestion_index: None,
        }
    }

    pub fn menu_next(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => {
                if i >= self.menu_items.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.menu_state.select(Some(i));
    }

    pub fn menu_previous(&mut self) {
        let i = match self.menu_state.selected() {
            Some(i) => {
                if i == 0 {
                    self.menu_items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.menu_state.select(Some(i));
    }

    pub fn suggestion_next(&mut self) {
        if self.suggestions.is_empty() {
            return;
        }
        let i = match self.suggestion_index {
            Some(i) => {
                if i >= self.suggestions.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.suggestion_index = Some(i);
    }

    pub fn suggestion_previous(&mut self) {
        if self.suggestions.is_empty() {
            return;
        }
        let i = match self.suggestion_index {
            Some(i) => {
                if i == 0 {
                    self.suggestions.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.suggestion_index = Some(i);
    }
}
