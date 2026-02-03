pub mod state;
pub mod ui;
pub mod utils;

use crate::commands::load::execute_load_tui;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*};
use state::{App, LoadStep, SearchCriteria, FilterStep, FocusPanel};
use std::io;
use std::path::Path;
use std::time::Duration;
use ui::ui;
use utils::{get_indexed_fields, get_path_suggestions, get_loaded_data};

pub fn run_interactive() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        if err.to_string() != "Quit" {
            println!("{:?}", err);
        }
    }
    Ok(())
}

fn run_app<B: Backend + io::Write>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let custom_purple = Color::Rgb(197, 137, 249);

    loop {
        terminal.draw(|f| ui(f, app, custom_purple))?;

        // Si estamos en estado de procesamiento, ejecutamos la tarea y luego limpiamos
        if let LoadStep::Processing = app.load_step {
             // Calcular posición Y para el spinner (Misma lógica que ui.rs)
            // Top Margin (2) + Menu (7) + LoadedData + Separator (1)
            let loaded_data = get_loaded_data();
            let loaded_height = if loaded_data.is_empty() {
                0
            } else {
                (loaded_data.len() as u16).min(8) + 1
            };
            
            // Y exacto donde empieza el bloque de input
            let spinner_y = 2 + 7 + loaded_height + 1;
            // X alineado con el margen: 4 (margin)
            let spinner_x = 4;

            // Ejecutar tarea bloqueante (CLI Spinner)
            let _ = execute_load_tui(
                &app.ndjson_path,
                &app.entity_name,
                &app.table_name,
                spinner_x,
                spinner_y
            );

            // Finalizamos la tarea y volvemos al menú principal
            app.active_task = None;
            app.load_step = LoadStep::InputPath;
            app.input_buffer.clear();
            app.ndjson_path.clear();
            app.entity_name.clear();
            app.table_name.clear();
            app.suggestions.clear();
            app.suggestion_index = None;
            
            // Forzar redibujado inmediato con el nuevo estado
            continue;
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press { continue; }

                if let Some(task) = app.active_task {
                    match task {
                        "Search" => handle_search_input(app, key),
                        "Load" => handle_load_input(app, key),
                        _ => {}
                    }
                } else {
                    handle_main_menu_input(app, key)?;
                }
            }
        }
    }
}

fn handle_main_menu_input(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Err(anyhow::anyhow!("Quit")),
        KeyCode::Down => app.menu_next(),
        KeyCode::Up => app.menu_previous(),
        KeyCode::Enter => match app.menu_state.selected() {
            Some(0) => {
                app.active_task = Some("Load");
                // Iniciar sugerencias desde ROOT inmediatamente al entrar
                app.suggestions = get_path_suggestions("");
                app.suggestion_index = if app.suggestions.is_empty() {
                    None
                } else {
                    Some(0)
                };
            }
            Some(1) => {
                app.active_task = Some("Search");
                app.focus_panel = FocusPanel::Left;
                app.search_entities.clear();
                
                // LEER ENTIDADES DESDE data/
                if let Ok(entries) = std::fs::read_dir("data") {
                    app.search_entities = entries.flatten()
                        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                }
                app.search_entities.sort();
                
                app.left_panel_state.select(Some(0));
                // Pre-seleccionar el primer item si existe
                app.middle_panel_state.select(Some(0));
                
                // Inicializar estados
                app.search_criteria = SearchCriteria::Entity;
                update_middle_panel_content(app);
            }
            Some(2) => return Err(anyhow::anyhow!("Quit")),
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn handle_load_input(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            // 1. Manejo de selección de sugerencia (Prioritario)
            if let Some(idx) = app.suggestion_index {
                if !app.suggestions.is_empty() && idx < app.suggestions.len() {
                    let selected = &app.suggestions[idx];

                    // Si es directorio (termina en /), navegamos
                    if selected.ends_with(std::path::MAIN_SEPARATOR) {
                        app.input_buffer = selected.clone();
                        app.suggestions = get_path_suggestions(&app.input_buffer);
                        app.suggestion_index = if app.suggestions.is_empty() {
                            None
                        } else {
                            Some(0)
                        };
                        return;
                    }
                    // Si es archivo .ndjson, LO SELECCIONAMOS Y AVANZAMOS
                    else if selected.ends_with(".ndjson") {
                        app.ndjson_path = selected.clone();
                        app.input_buffer.clear();
                        app.suggestions.clear();
                        app.suggestion_index = None;
                        app.load_step = LoadStep::InputEntity;
                        return;
                    }
                }
            }

            // 2. Manejo normal de Enter
            match app.load_step {
                LoadStep::InputPath => {
                    if !app.input_buffer.is_empty() {
                        app.ndjson_path = app.input_buffer.clone();
                        app.input_buffer.clear();
                        app.suggestions.clear();
                        app.suggestion_index = None;
                        app.load_step = LoadStep::InputEntity;
                    }
                }
                LoadStep::InputEntity => {
                    app.entity_name = app.input_buffer.clone();
                    app.input_buffer.clear();
                    app.load_step = LoadStep::InputTable;
                }
                LoadStep::InputTable => {
                    app.table_name = app.input_buffer.clone();
                    app.input_buffer.clear();
                    app.load_step = LoadStep::Processing;
                }
                _ => {}
            }
        }
        KeyCode::Char(c) => {
            app.input_buffer.push(c);
            if let LoadStep::InputPath = app.load_step {
                app.suggestions = get_path_suggestions(&app.input_buffer);
                app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
            }
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
            if let LoadStep::InputPath = app.load_step {
                app.suggestions = get_path_suggestions(&app.input_buffer);
                app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
            }
        }
        KeyCode::Esc => {
            app.active_task = None;
            app.input_buffer.clear();
            app.suggestions.clear();
            app.suggestion_index = None;
            app.load_step = LoadStep::InputPath;
        }
        KeyCode::Up => {
            if !app.suggestions.is_empty() { app.suggestion_previous(); }
        }
        KeyCode::Down => {
            if !app.suggestions.is_empty() { app.suggestion_next(); }
        }
        KeyCode::Tab => {
            if let Some(idx) = app.suggestion_index {
                if idx < app.suggestions.len() {
                    let selected = &app.suggestions[idx];
                    app.input_buffer = selected.clone();
                    if selected.ends_with(std::path::MAIN_SEPARATOR) {
                        app.suggestions = get_path_suggestions(&app.input_buffer);
                        app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
                    }
                }
            }
        }
        _ => {}
    }
}

fn handle_search_input(app: &mut App, key: event::KeyEvent) {
    if app.focus_panel == FocusPanel::Bottom {
        match key.code {
            KeyCode::Enter => app.focus_panel = FocusPanel::Middle,
            KeyCode::Esc => {
                app.filter_value_input.clear();
                app.focus_panel = FocusPanel::Middle;
            },
            KeyCode::Char(c) => app.filter_value_input.push(c),
            KeyCode::Backspace => { app.filter_value_input.pop(); },
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.active_task = None;
            // Limpiar todo el estado de búsqueda
            app.selected_entity = None;
            app.selected_table = None;
            app.selected_field = None;
            app.filter_value_input.clear();
            app.search_tables.clear();
            app.available_fields.clear();
            app.focus_panel = FocusPanel::Left;
            app.search_criteria = SearchCriteria::Entity;
            app.left_panel_state.select(Some(0));
            app.middle_panel_state.select(Some(0));
            app.right_panel_state.select(Some(0));
        }
        KeyCode::Right | KeyCode::Tab => match app.focus_panel {
            FocusPanel::Left => app.focus_panel = FocusPanel::Middle,
            FocusPanel::Middle => if app.search_criteria == SearchCriteria::Filters { app.focus_panel = FocusPanel::Right },
            _ => {}
        },
        KeyCode::Left => match app.focus_panel {
            FocusPanel::Middle => app.focus_panel = FocusPanel::Left,
            FocusPanel::Right => app.focus_panel = FocusPanel::Middle,
            _ => {}
        },
        KeyCode::Up => navigate_list(app, -1),
        KeyCode::Down => navigate_list(app, 1),
        KeyCode::Enter => {
            match (app.focus_panel, app.search_criteria) {
                (FocusPanel::Middle, SearchCriteria::Entity) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let new_selection = app.search_entities.get(idx).cloned();
                        if app.selected_entity == new_selection {
                            app.selected_entity = None;
                        } else {
                            app.selected_entity = new_selection;
                        }
                        // Reset everything dependent on entity
                        app.selected_table = None;
                        app.available_fields.clear();
                        app.selected_field = None;
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Table) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let new_selection = app.search_tables.get(idx).cloned();
                        if app.selected_table == new_selection {
                            app.selected_table = None;
                        } else {
                            app.selected_table = new_selection;
                        }
                        app.selected_field = None;
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Filters) => {
                    if app.filter_step == FilterStep::Value {
                        app.focus_panel = FocusPanel::Bottom;
                    }
                }
                (FocusPanel::Right, SearchCriteria::Filters) => {
                    if app.filter_step == FilterStep::Field {
                        if let Some(idx) = app.right_panel_state.selected() {
                            app.selected_field = app.available_fields.get(idx).cloned();
                        }
                    }
                }
                _ => {}
            }
        },
        _ => {}
    }
}

fn navigate_list(app: &mut App, delta: isize) {
    let (state, len) = match app.focus_panel {
        FocusPanel::Left => (&mut app.left_panel_state, 3),
        FocusPanel::Middle => {
            let len = match app.search_criteria {
                SearchCriteria::Entity => app.search_entities.len(),
                SearchCriteria::Table => app.search_tables.len(),
                SearchCriteria::Filters => 3,
            };
            (&mut app.middle_panel_state, len)
        },
        FocusPanel::Right => {
            let len = if app.search_criteria == SearchCriteria::Filters {
                match app.filter_step {
                    FilterStep::Field => app.available_fields.len(),
                    _ => 1,
                }
            } else { 0 };
            (&mut app.right_panel_state, len)
        },
        _ => return,
    };

    if len == 0 { if delta != 0 { return; } }
    let current = state.selected().unwrap_or(0);
    let next = if len > 0 { (current as isize + delta + len as isize) as usize % len } else { 0 };

    // Update derived state based on new selection
    match app.focus_panel {
        FocusPanel::Left => {
            let next_criteria = match next { 0 => SearchCriteria::Entity, 1 => SearchCriteria::Table, _ => SearchCriteria::Filters };
            
            // Requisitos para navegar:
            // Para ir a Table, debe haber Entity seleccionado
            if next_criteria == SearchCriteria::Table && app.selected_entity.is_none() {
                return;
            }
            // Para ir a Filters, debe haber Table seleccionado
            if next_criteria == SearchCriteria::Filters && app.selected_table.is_none() {
                return;
            }

            state.select(Some(next));
            app.search_criteria = next_criteria;
            app.middle_panel_state.select(Some(0));
            app.right_panel_state.select(Some(0));
            // Update the middle panel content based on the new criteria
            update_middle_panel_content(app);
        },
        FocusPanel::Middle => {
            state.select(Some(next));
            if app.search_criteria == SearchCriteria::Filters {
                 app.filter_step = match next { 0 => FilterStep::Field, 1 => FilterStep::Op, _ => FilterStep::Value };
                 app.right_panel_state.select(Some(0));
            }
        },
        _ => {
            state.select(Some(next));
        }
    }
}

// Helper to update content dependent on selection without recursion
fn update_middle_panel_content(app: &mut App) {
    match app.search_criteria {
        SearchCriteria::Entity => {
            // No auto-select entity here.
            // Just ensure tables are loaded for the CURRENTLY selected entity if any.
            app.search_tables.clear();
            if let Some(entity) = &app.selected_entity {
                let path = format!("data/{}", entity);
                if let Ok(tables) = std::fs::read_dir(path) {
                    app.search_tables = tables.flatten()
                        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                }
            }
            app.search_tables.sort();
        },
        SearchCriteria::Table => {
             // No auto-select table here.
             // Just ensure fields are loaded for the CURRENTLY selected entity and table.
            if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                app.available_fields = get_indexed_fields(Path::new("data"), e, t);
            } else {
                app.available_fields.clear();
            }
        },
        SearchCriteria::Filters => {
            app.right_panel_state.select(Some(0));
        }
    }
}
