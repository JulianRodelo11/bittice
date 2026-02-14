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
use tokio::sync::{mpsc, oneshot};

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

            // LIMPIEZA POST-CARGA: Forzar a Ratatui a redibujar todo
            let _ = terminal.clear(); 

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

        // Poll Server Logs
        if let Some(rx) = &mut app.server_log_receiver {
             while let Ok(msg) = rx.try_recv() {
                 app.server_logs.push(msg);
             }
        }

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press { continue; }

                    if let Some(task) = app.active_task {
                        match task {
                            "Search" => search::handle_search_input(app, key),
                            "Load" => handle_load_input(app, key),
                            "Server" => handle_server_input(app, key),
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
                                if mouse.modifiers.contains(event::KeyModifiers::SHIFT) {
                                    app.results_scroll_x = app.results_scroll_x.saturating_sub(5);
                                } else {
                                    app.results_scroll = app.results_scroll.saturating_sub(1);
                                }
                            },
                            event::MouseEventKind::ScrollDown => {
                                if mouse.modifiers.contains(event::KeyModifiers::SHIFT) {
                                    app.results_scroll_x = app.results_scroll_x.saturating_add(5);
                                } else {
                                    app.results_scroll = app.results_scroll.saturating_add(1);
                                }
                            },
                            event::MouseEventKind::ScrollLeft => {
                                app.results_scroll_x = app.results_scroll_x.saturating_sub(5);
                            },
                            event::MouseEventKind::ScrollRight => {
                                app.results_scroll_x = app.results_scroll_x.saturating_add(5);
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
                app.status_message = None; // Limpiar mensajes previos
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
            Some(2) => {
                // Start Local Server
                app.active_task = Some("Server");
                let (log_tx, log_rx) = mpsc::channel(100);
                app.server_log_receiver = Some(log_rx);
                app.server_logs.clear();
                
                let (shutdown_tx, shutdown_rx) = oneshot::channel();
                app.server_shutdown_tx = Some(shutdown_tx);
                app.is_server_running = true;
                app.server_focus = crate::repl::state::ServerFocus::Endpoints;
                app.endpoint_state.select(Some(0));
                app.log_state.select(Some(0));

                // Spawn server thread
                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(crate::server::start_server(log_tx, shutdown_rx));
                });
            }
            Some(3) => return Err(anyhow::anyhow!("Quit")),
            _ => {}
        },
        _ => {}
    }
    Ok(())
}

fn handle_server_input(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if let Some(tx) = app.server_shutdown_tx.take() {
                let _ = tx.send(());
            }
            app.is_server_running = false;
            app.active_task = None;
        }
        KeyCode::Tab => {
            app.server_focus = match app.server_focus {
                crate::repl::state::ServerFocus::Endpoints => crate::repl::state::ServerFocus::Logs,
                crate::repl::state::ServerFocus::Logs => crate::repl::state::ServerFocus::Endpoints,
            };
        }
        KeyCode::Down => {
            match app.server_focus {
                crate::repl::state::ServerFocus::Endpoints => {
                    let queries = crate::core::saved_queries::load_queries().unwrap_or_default();
                    if !queries.is_empty() {
                        let i = match app.endpoint_state.selected() {
                            Some(i) => if i >= queries.len() - 1 { 0 } else { i + 1 },
                            None => 0,
                        };
                        app.endpoint_state.select(Some(i));
                    }
                }
                crate::repl::state::ServerFocus::Logs => {
                    if !app.server_logs.is_empty() {
                        let i = match app.log_state.selected() {
                            Some(i) => if i >= app.server_logs.len() - 1 { 0 } else { i + 1 },
                            None => 0,
                        };
                        app.log_state.select(Some(i));
                    }
                }
            }
        }
        KeyCode::Up => {
            match app.server_focus {
                crate::repl::state::ServerFocus::Endpoints => {
                    let queries = crate::core::saved_queries::load_queries().unwrap_or_default();
                    if !queries.is_empty() {
                        let i = match app.endpoint_state.selected() {
                            Some(i) => if i == 0 { queries.len() - 1 } else { i - 1 },
                            None => 0,
                        };
                        app.endpoint_state.select(Some(i));
                    }
                }
                crate::repl::state::ServerFocus::Logs => {
                    if !app.server_logs.is_empty() {
                        let i = match app.log_state.selected() {
                            Some(i) => if i == 0 { app.server_logs.len() - 1 } else { i - 1 },
                            None => 0,
                        };
                        app.log_state.select(Some(i));
                    }
                }
            }
        }
        KeyCode::Char('c') => {
            let mut clipboard = arboard::Clipboard::new().unwrap();
            match app.server_focus {
                crate::repl::state::ServerFocus::Endpoints => {
                    let queries = crate::core::saved_queries::load_queries().unwrap_or_default();
                    if let Some(i) = app.endpoint_state.selected() {
                        if let Some(q) = queries.get(i) {
                            let text = format!("http://127.0.0.1:3000/{}", q.name);
                            let _ = clipboard.set_text(text);
                            app.status_message = Some(("Endpoint copied to clipboard!".to_string(), true));
                        }
                    }
                }
                crate::repl::state::ServerFocus::Logs => {
                    if let Some(i) = app.log_state.selected() {
                        // Logs are rev() in UI, so we need to match that
                        let logs: Vec<_> = app.server_logs.iter().rev().collect();
                        if let Some(log) = logs.get(i) {
                            let _ = clipboard.set_text((*log).clone());
                            app.status_message = Some(("Log line copied to clipboard!".to_string(), true));
                        }
                    }
                }
            }
        }
        _ => {}
    }
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


