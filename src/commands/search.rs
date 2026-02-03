use crossterm::event::{self, KeyCode};
use std::path::Path;

use crate::repl::state::{App, SearchCriteria, FilterStep, FocusPanel};
use crate::repl::utils::get_indexed_fields;

/// Inicializa el estado para la búsqueda: carga entidades y resetea paneles.
pub fn init_search(app: &mut App) {
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

pub fn handle_search_input(app: &mut App, key: event::KeyEvent) {
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

pub fn update_middle_panel_content(app: &mut App) {
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
