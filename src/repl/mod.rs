pub mod state;
pub mod ui;
pub mod utils;

use crate::commands::load::execute_load_tui;
use crate::commands::search;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*};
use state::{App, LoadStep};
use std::io;
use std::time::Duration;
use ui::ui;
use crate::ui::colors;
use utils::{get_path_suggestions, get_loaded_data};

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
        terminal.draw(|f| ui(f, app, colors::PRIMARY_COLOR))?;

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
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press { continue; }

                    if let Some(task) = app.active_task {
                        match task {
                            "Search" => search::handle_search_input(app, key),
                            "Load" => handle_load_input(app, key),
                            _ => {}
                        }
                    } else {
                        handle_main_menu_input(app, key)?;
                    }
                },
                Event::Mouse(mouse) => {
                    if app.active_task == Some("Search") && app.search_results.is_some() {
                        match mouse.kind {
                            event::MouseEventKind::ScrollUp => {
                                app.results_scroll = app.results_scroll.saturating_sub(1);
                            },
                            event::MouseEventKind::ScrollDown => {
                                app.results_scroll = app.results_scroll.saturating_add(1);
                            },
                            _ => {}
                        }
                    }
                },
                _ => {}
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
                search::init_search(app);
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


