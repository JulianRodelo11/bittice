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
use utils::{get_indexed_fields, get_path_suggestions};

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
    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.active_task == Some("Load") && app.load_step == LoadStep::Processing {
            terminal.draw(|f| ui(f, app))?;
            let _ = execute_load_tui(&app.ndjson_path, &app.entity_name, &app.table_name, 0, 0);
            app.active_task = None;
            app.load_step = LoadStep::InputPath;
            app.input_buffer.clear();
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
            Some(0) => app.active_task = Some("Load"),
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
                // No pre-seleccionar nada en el panel medio
                app.middle_panel_state.select(None);
                
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
        KeyCode::Char(c) => {
            app.input_buffer.push(c);
            if app.load_step == LoadStep::InputPath {
                app.suggestions = get_path_suggestions(&app.input_buffer);
                app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
            }
        }
        KeyCode::Backspace => {
            app.input_buffer.pop();
             if app.load_step == LoadStep::InputPath {
                app.suggestions = get_path_suggestions(&app.input_buffer);
                app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
            }
        }
        KeyCode::Enter => {
            if let Some(index) = app.suggestion_index {
                app.input_buffer = app.suggestions[index].clone();
            }

            match app.load_step {
                LoadStep::InputPath => {
                    app.ndjson_path = app.input_buffer.clone();
                    app.load_step = LoadStep::InputEntity;
                }
                LoadStep::InputEntity => {
                    app.entity_name = app.input_buffer.clone();
                    app.load_step = LoadStep::InputTable;
                }
                LoadStep::InputTable => {
                    app.table_name = app.input_buffer.clone();
                    app.load_step = LoadStep::Processing;
                }
                _ => {}
            }
            app.input_buffer.clear();
            app.suggestions.clear();
            app.suggestion_index = None;
        }
        KeyCode::Esc => {
            app.active_task = None;
            app.input_buffer.clear();
            app.suggestions.clear();
            app.suggestion_index = None;
        }
        KeyCode::Tab => {
            if !app.suggestions.is_empty() {
                let s_index = app.suggestion_index.unwrap_or(0);
                app.input_buffer = app.suggestions[s_index].clone();
            }
        }
        KeyCode::Down => {
            if !app.suggestions.is_empty() {
                app.suggestion_next();
            }
        }
        KeyCode::Up => {
            if !app.suggestions.is_empty() {
                app.suggestion_previous();
            }
        },
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
        KeyCode::Esc => app.active_task = None,
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
            if app.focus_panel == FocusPanel::Middle && app.search_criteria == SearchCriteria::Filters && app.filter_step == FilterStep::Value {
                app.focus_panel = FocusPanel::Bottom;
            } else if app.focus_panel == FocusPanel::Right {
                 if app.search_criteria == SearchCriteria::Filters && app.filter_step == FilterStep::Field {
                    if let Some(idx) = app.right_panel_state.selected() {
                        app.selected_field = app.available_fields.get(idx).cloned();
                    }
                 }
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
    state.select(Some(next));

    // Update derived state based on new selection
    match app.focus_panel {
        FocusPanel::Left => {
            app.search_criteria = match next { 0 => SearchCriteria::Entity, 1 => SearchCriteria::Table, _ => SearchCriteria::Filters };
            app.middle_panel_state.select(Some(0));
            app.right_panel_state.select(Some(0));
            // Update the middle panel content based on the new criteria
            update_middle_panel_content(app);
        },
        FocusPanel::Middle => {
            if app.search_criteria == SearchCriteria::Filters {
                 app.filter_step = match next { 0 => FilterStep::Field, 1 => FilterStep::Op, _ => FilterStep::Value };
                 app.right_panel_state.select(Some(0));
            } else {
                 update_middle_panel_content(app);
            }
        },
        _ => {}
    }
}

// Helper to update content dependent on selection without recursion
fn update_middle_panel_content(app: &mut App) {
    match app.search_criteria {
        SearchCriteria::Entity => {
            // Update selected entity based on middle panel selection
            if let Some(idx) = app.middle_panel_state.selected() {
                 app.selected_entity = app.search_entities.get(idx).cloned();
            }
            // Reload tables for selected entity: data/{selected_entity}
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
             // Update selected table based on middle panel selection
            if let Some(idx) = app.middle_panel_state.selected() {
                 app.selected_table = app.search_tables.get(idx).cloned();
            }
            // Reload fields
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