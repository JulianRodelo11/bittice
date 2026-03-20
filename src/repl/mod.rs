pub mod state;
pub mod ui;
pub mod utils;
pub mod view;
pub mod startup;

use crate::commands::load::execute_load_tui;
use crate::commands::search;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::Terminal;
use state::{App, LoadStep};
use std::io;
use std::path::Path;
use std::time::Duration;
use ui::ui;
use crate::ui::colors;
use crate::repl::utils::{get_path_suggestions, get_loaded_data};
use tokio::sync::{mpsc, oneshot};
use view::{handle_bittice_input};

pub fn run_interactive(app: Option<App>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, event::EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = app.unwrap_or_else(App::new);
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

        if let LoadStep::Processing = app.load_step {
            let loaded_data = get_loaded_data();
            let loaded_height = if loaded_data.is_empty() { 0 } else { (loaded_data.len() as u16).min(8) + 1 };
            let spinner_y = 2 + 7 + loaded_height + 1;
            let spinner_x = 4;

            let _ = execute_load_tui(
                &app.ndjson_path,
                &app.entity_name,
                &app.table_name,
                spinner_x,
                spinner_y
            );

            let _ = terminal.clear(); 

            app.active_task = None;
            app.load_step = LoadStep::InputPath;
            app.input_buffer.clear();
            app.ndjson_path.clear();
            app.entity_name.clear();
            app.table_name.clear();
            app.suggestions.clear();
            app.suggestion_index = None;
            continue;
        }

        if let Some(rx) = &mut app.server_log_receiver {
             while let Ok(msg) = rx.try_recv() {
                 app.server_logs.push(msg);
             }
        }

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;
            
            if app.active_task == Some("Bittice") {
                if let Event::Key(key) = ev {
                    if key.kind != KeyEventKind::Press { continue; }
                }
                handle_bittice_input(app, ev)?;
                continue;
            }

            match ev {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press { continue; }

                    if let Some(task) = app.active_task {
                        match task {
                            "Startup" => handle_startup_input(app, key)?,
                            "Search" | "Create" | "Update" | "Delete" | "Batch" => search::handle_search_input(app, key),
                            "Load" => handle_load_input(app, key),
                            "Server" => handle_server_input(app, key),
                            "Bittice" => {
                                if let Event::Key(key_ev) = ev {
                                    handle_bittice_input(app, Event::Key(key_ev))?;
                                }
                            }
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

fn handle_startup_input(app: &mut App, key: event::KeyEvent) -> Result<()> {
    use crate::repl::state::StartupStep;

    match app.startup_step {
        StartupStep::Selection => {
            match key.code {
                KeyCode::Esc => return Err(anyhow::anyhow!("Quit")),
                KeyCode::Down => app.startup_menu_next(),
                KeyCode::Up => app.startup_menu_previous(),
                KeyCode::Enter => match app.startup_menu_state.selected() {
                    Some(0) => {
                        app.startup_step = StartupStep::Host;
                        app.input_buffer = app.cdc_info.host.clone();
                    }
                    Some(1) => {
                        app.active_task = None; // Ir al menú principal
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        StartupStep::Host | StartupStep::Port | StartupStep::User | StartupStep::Password | StartupStep::Database | StartupStep::Entity => {
            match key.code {
                KeyCode::Esc => {
                    app.startup_step = StartupStep::Selection;
                    app.input_buffer.clear();
                }
                KeyCode::Enter => {
                    match app.startup_step {
                        StartupStep::Host => {
                            if !app.input_buffer.is_empty() { app.cdc_info.host = app.input_buffer.clone(); }
                            app.startup_step = StartupStep::Port;
                            app.input_buffer = app.cdc_info.port.clone();
                        }
                        StartupStep::Port => {
                            if !app.input_buffer.is_empty() { app.cdc_info.port = app.input_buffer.clone(); }
                            app.startup_step = StartupStep::User;
                            app.input_buffer = app.cdc_info.user.clone();
                        }
                        StartupStep::User => {
                            if !app.input_buffer.is_empty() { app.cdc_info.user = app.input_buffer.clone(); }
                            app.startup_step = StartupStep::Password;
                            app.input_buffer.clear();
                        }
                        StartupStep::Password => {
                            app.cdc_info.pass = app.input_buffer.clone();
                            app.startup_step = StartupStep::Database;
                            app.input_buffer.clear();
                        }
                        StartupStep::Database => {
                            app.cdc_info.database = app.input_buffer.clone();
                            app.startup_step = StartupStep::Entity;
                            app.input_buffer = app.cdc_info.database.clone();
                        }
                        StartupStep::Entity => {
                            if !app.input_buffer.is_empty() { app.cdc_info.entity = app.input_buffer.clone(); }
                            app.input_buffer.clear();
                            app.startup_step = StartupStep::CdcRunning;
                            
                            // Construir URL y lanzar CDC
                            let url = format!("mysql://{}:{}@{}:{}/{}", 
                                app.cdc_info.user, app.cdc_info.pass, 
                                app.cdc_info.host, app.cdc_info.port, 
                                app.cdc_info.database);
                            
                            let entity = app.cdc_info.entity.clone();
                            let database = app.cdc_info.database.clone();
                            
                            let (log_tx, log_rx) = mpsc::channel(100);
                            app.server_log_receiver = Some(log_rx);
                            app.server_logs.clear();
                            
                            std::thread::spawn(move || {
                                let rt = tokio::runtime::Runtime::new().unwrap();
                                let worker = crate::core::cdc::CdcWorker::with_log(url, entity, database, Some(log_tx));
                                if let Err(e) = rt.block_on(worker.run()) {
                                    eprintln!("CDC Worker error: {}", e);
                                }
                            });
                        }
                        _ => {}
                    }
                }
                KeyCode::Char(c) => app.input_buffer.push(c),
                KeyCode::Backspace => { app.input_buffer.pop(); }
                _ => {}
            }
        }
        StartupStep::CdcRunning => {
            if key.code == KeyCode::Esc {
                app.active_task = None; // Volver al menú principal
            } else if key.code == KeyCode::Enter {
                app.startup_step = StartupStep::DockerBuild;
            }
        }
        StartupStep::DockerBuild => {
            match key.code {
                KeyCode::Esc => { app.startup_step = StartupStep::CdcRunning; }
                KeyCode::Enter => {
                    // Solo iniciamos el build si no está ya en curso
                    if app.docker_build_status.is_none() || app.docker_build_status.as_ref().map(|s| s.contains("Error")).unwrap_or(false) {
                        app.docker_build_status = Some("Iniciando build de Docker...".to_string());
                        
                        let entity = app.cdc_info.entity.clone();
                        let (log_tx, _log_rx) = mpsc::channel(100);
                        let log_tx_clone = log_tx.clone();
                        
                        // Capturar logs del build para mostrarlos en el TUI
                        std::thread::spawn(move || {
                            use std::process::{Command, Stdio};
                            use std::io::{BufRead, BufReader};
                            
                            let mut child = Command::new("docker")
                                .args(["build", "-t", &format!("bittice-{}", entity), "."])
                                .stdout(Stdio::piped())
                                .stderr(Stdio::piped())
                                .spawn()
                                .expect("Falló al iniciar comando Docker");

                            let stdout = child.stdout.take().unwrap();
                            let stderr = child.stderr.take().unwrap();
                            let log_tx_inner = log_tx_clone.clone();

                            // Thread para stdout
                            let tx_out = log_tx_inner.clone();
                            std::thread::spawn(move || {
                                let reader = BufReader::new(stdout);
                                for line in reader.lines().flatten() {
                                    let _ = tx_out.try_send(format!(" [docker] {}", line));
                                }
                            });

                            // Thread para stderr
                            let tx_err = log_tx_inner.clone();
                            std::thread::spawn(move || {
                                let reader = BufReader::new(stderr);
                                for line in reader.lines().flatten() {
                                    let _ = tx_err.try_send(format!(" [docker-err] {}", line));
                                }
                            });

                            let status = child.wait().expect("Error esperando a Docker");
                            if status.success() {
                                let _ = log_tx_inner.try_send("✓ Imagen de Docker creada con éxito.".to_string());
                            } else {
                                let _ = log_tx_inner.try_send("✗ Error al crear imagen de Docker.".to_string());
                            }
                        });

                        // El receptor de estos logs ya existe en run_app (app.server_log_receiver)
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

fn handle_main_menu_input(app: &mut App, key: event::KeyEvent) -> Result<()> {
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Err(anyhow::anyhow!("Quit")),
        KeyCode::Down => app.menu_next(),
        KeyCode::Up => app.menu_previous(),
        KeyCode::Enter => match app.menu_state.selected() {
            Some(0) => {
                app.active_task = Some("Load");
                app.status_message = None;
                app.suggestions = get_path_suggestions("./");
                app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
            }
            Some(1) => {
                search::init_crud(app, crate::repl::state::SearchCriteria::Create);
            }
            Some(2) => {
                search::init_search(app);
            }
            Some(3) => {
                search::init_crud(app, crate::repl::state::SearchCriteria::Update);
            }
            Some(4) => {
                search::init_crud(app, crate::repl::state::SearchCriteria::Delete);
            }
            Some(5) => {
                app.active_task = Some("Batch");
                app.show_saved_queries = true;
                app.is_loading_to_edit = false;
                app.batch_selected_ops.clear();
                if !app.saved_queries.is_empty() {
                    app.saved_queries_state.select(Some(0));
                }
            }
            Some(6) => {
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

                std::thread::spawn(move || {
                    let rt = tokio::runtime::Runtime::new().unwrap();
                    rt.block_on(crate::server::start_server(log_tx, shutdown_rx));
                });
            }
            Some(7) => {
                app.active_task = Some("Bittice");
            }
            Some(8) => return Err(anyhow::anyhow!("Quit")),
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
                    let ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                    if !ops.is_empty() {
                        let i = match app.endpoint_state.selected() {
                            Some(i) => if i >= ops.len() - 1 { 0 } else { i + 1 },
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
                    let ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                    if !ops.is_empty() {
                        let i = match app.endpoint_state.selected() {
                            Some(i) => if i == 0 { ops.len() - 1 } else { i - 1 },
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
                    let ops = crate::core::saved_queries::load_operations().unwrap_or_default();
                    if let Some(i) = app.endpoint_state.selected() {
                        if let Some(op) = ops.get(i) {
                            let mut params = Vec::new();
                            let name = op.name().to_string();

                            let method = match op {
                                crate::core::saved_queries::SavedOperation::Read(q) => {
                                    for f in &q.filters {
                                        if f.value.starts_with('$') {
                                            params.push(f.value[1..].to_string());
                                        }
                                    }
                                    if let Some(ref p) = q.limit_param {
                                        if let Some(k) = p.strip_prefix('$') {
                                            params.push(k.to_string());
                                        }
                                    }
                                    for agg in &q.aggregations {
                                        if let Some(obj) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                                            for val in obj.values() {
                                                if let Some(s) = val.as_str() {
                                                    if s.starts_with('$') {
                                                        params.push(s[1..].to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "GET"
                                },
                                crate::core::saved_queries::SavedOperation::Insert(_) => "POST",
                                crate::core::saved_queries::SavedOperation::Update(_) => "PUT",
                                crate::core::saved_queries::SavedOperation::Delete(_) => "DELETE",
                                crate::core::saved_queries::SavedOperation::Batch(b) => {
                                    for op_name in &b.operations {
                                        if let Some(crate::core::saved_queries::SavedOperation::Read(q)) = ops.iter().find(|o| o.name() == op_name) {
                                            for f in &q.filters {
                                                if f.value.starts_with('$') {
                                                    params.push(f.value[1..].to_string());
                                                }
                                            }
                                            if let Some(ref p) = q.limit_param {
                                                if let Some(k) = p.strip_prefix('$') {
                                                    params.push(k.to_string());
                                                }
                                            }
                                            for agg in &q.aggregations {
                                                if let Some(obj) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                                                    for val in obj.values() {
                                                        if let Some(s) = val.as_str() {
                                                            if s.starts_with('$') {
                                                                params.push(s[1..].to_string());
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    "GET"
                                }
                            };
                            
                            params.sort();
                            params.dedup();
                            
                            let host = std::env::var("BITTICE_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
                            let mut url = format!("http://{}:3000/{}", host, name);
                            if method == "GET" && !params.is_empty() {
                                let query_string = params.iter().map(|p| format!("{}=?", p)).collect::<Vec<_>>().join("&");
                                url.push('?');
                                url.push_str(&query_string);
                            }

                            let _ = clipboard.set_text(url);
                            app.status_message = Some((format!("{} endpoint copied to clipboard!", method), true));
                        }
                    }
                }
                crate::repl::state::ServerFocus::Logs => {
                    if let Some(i) = app.log_state.selected() {
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
            if let Some(idx) = app.suggestion_index {
                if !app.suggestions.is_empty() && idx < app.suggestions.len() {
                    let selected = &app.suggestions[idx];
                    if selected.ends_with(std::path::MAIN_SEPARATOR) {
                        app.input_buffer = selected.clone();
                        app.suggestions = get_path_suggestions(&app.input_buffer);
                        app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
                        return;
                    }
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
                    if app.input_buffer.is_empty() {
                        let path = Path::new(&app.ndjson_path);
                        app.entity_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("default").to_string();
                    } else {
                        app.entity_name = app.input_buffer.clone();
                    }
                    app.input_buffer.clear();
                    app.load_step = LoadStep::InputTable;
                }
                LoadStep::InputTable => {
                    if app.input_buffer.is_empty() {
                        let path = Path::new(&app.ndjson_path);
                        app.table_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("records").to_string();
                    } else {
                        app.table_name = app.input_buffer.clone();
                    }
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
