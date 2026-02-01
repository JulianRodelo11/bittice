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
use ratatui::{prelude::*, style::Color};
use state::{App, LoadStep};
use std::io;
use std::time::Duration;
use ui::ui;
use utils::get_path_suggestions;

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
        println!("{:?}", err)
    }
    Ok(())
}

fn run_app<B: Backend + io::Write>(terminal: &mut Terminal<B>, app: &mut App) -> Result<()> {
    let custom_purple = Color::Rgb(197, 137, 249);

    loop {
        terminal.draw(|f| ui(f, app, custom_purple))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // Si hay una tarea activa y estamos en modo Input, capturamos texto
                    if app.active_task.is_some() {
                        match key.code {
                            KeyCode::Enter => {
                                // 1. Manejo de selección de sugerencia (Prioritario)
                                if let Some(idx) = app.suggestion_index {
                                    if !app.suggestions.is_empty() && idx < app.suggestions.len() {
                                        let selected = &app.suggestions[idx];

                                        // Si es directorio (termina en /), navegamos
                                        if selected.ends_with(std::path::MAIN_SEPARATOR) {
                                            app.input_buffer = selected.clone();
                                            app.suggestions =
                                                get_path_suggestions(&app.input_buffer);
                                            app.suggestion_index = if app.suggestions.is_empty() {
                                                None
                                            } else {
                                                Some(0)
                                            };
                                            continue; // No procesamos más, seguimos en InputPath
                                        }
                                        // Si es archivo .ndjson, LO SELECCIONAMOS Y AVANZAMOS
                                        else if selected.ends_with(".ndjson") {
                                            app.ndjson_path = selected.clone();
                                            // Limpiamos todo para el siguiente paso
                                            app.input_buffer.clear();
                                            app.suggestions.clear();
                                            app.suggestion_index = None;
                                            app.load_step = LoadStep::InputEntity;
                                            continue;
                                        }
                                    }
                                }

                                // 2. Manejo normal de Enter (Si escribió manual o confirmó)
                                match app.load_step {
                                    LoadStep::InputPath => {
                                        // Solo avanzamos si el path parece válido (no vacío)
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

                                        // Aquí llamarías a tu lógica de bittice::core::writer
                                        match execute_load_tui(
                                            &app.ndjson_path,
                                            &app.entity_name,
                                            &app.table_name,
                                        ) {
                                            _ => {}
                                        }

                                        // Finalizamos la tarea y volvemos al menú principal
                                        app.active_task = None;
                                        app.load_step = LoadStep::InputPath;
                                        app.input_buffer.clear();
                                        app.ndjson_path.clear();
                                        app.entity_name.clear();
                                        app.table_name.clear();
                                        app.suggestions.clear();
                                        app.suggestion_index = None;
                                    }
                                    LoadStep::Done => {
                                        // Este estado ya no se alcanza con la lógica nueva,
                                        // pero lo mantenemos por consistencia del enum
                                        app.active_task = None;
                                        app.load_step = LoadStep::InputPath;
                                    }
                                    _ => {}
                                }
                            }
                            KeyCode::Char(c) => {
                                app.input_buffer.push(c);
                                // Actualizar sugerencias solo en InputPath
                                if let LoadStep::InputPath = app.load_step {
                                    app.suggestions = get_path_suggestions(&app.input_buffer);
                                    app.suggestion_index = if app.suggestions.is_empty() {
                                        None
                                    } else {
                                        Some(0)
                                    };
                                }
                            }
                            KeyCode::Backspace => {
                                app.input_buffer.pop();
                                if let LoadStep::InputPath = app.load_step {
                                    app.suggestions = get_path_suggestions(&app.input_buffer);
                                    app.suggestion_index = if app.suggestions.is_empty() {
                                        None
                                    } else {
                                        Some(0)
                                    };
                                }
                            }
                            KeyCode::Esc => {
                                app.active_task = None;
                                app.input_buffer.clear();
                                app.suggestions.clear();
                                app.suggestion_index = None;
                            }
                            KeyCode::Up => {
                                if !app.suggestions.is_empty() {
                                    app.suggestion_previous();
                                }
                            }
                            KeyCode::Down => {
                                if !app.suggestions.is_empty() {
                                    app.suggestion_next();
                                }
                            }
                            KeyCode::Tab => {
                                // Tab completa directorio o selecciona archivo
                                if let Some(idx) = app.suggestion_index {
                                    if idx < app.suggestions.len() {
                                        let selected = &app.suggestions[idx];
                                        if selected.ends_with(std::path::MAIN_SEPARATOR) {
                                            app.input_buffer = selected.clone();
                                            app.suggestions =
                                                get_path_suggestions(&app.input_buffer);
                                            app.suggestion_index = if app.suggestions.is_empty() {
                                                None
                                            } else {
                                                Some(0)
                                            };
                                        } else {
                                            app.input_buffer = selected.clone();
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        // Navegación del menú principal
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Down => app.menu_next(),
                            KeyCode::Up => app.menu_previous(),
                            KeyCode::Enter => {
                                match app.menu_state.selected() {
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
                                    Some(1) => app.active_task = Some("Search"),
                                    Some(2) => return Ok(()),
                                    _ => {}
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
