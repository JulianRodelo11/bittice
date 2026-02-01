use std::io;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use ratatui::layout::Margin;
use std::time::Duration;
use crate::commands::load::execute_load_tui;

use std::path::Path;

// Pasos para el proceso de carga
enum LoadStep {
    InputPath,
    InputEntity,
    InputTable,
    Processing,
    Done,
}

struct App {
    // Estado del menú superior
    menu_items: Vec<&'static str>,
    menu_state: ListState,
    
    // Estado de la tarea activa
    active_task: Option<&'static str>, // None, Some("Load"), Some("Search")
    
    // Estado de Input para el contenedor de abajo
    input_buffer: String,
    load_step: LoadStep,
    ndjson_path: String,
    entity_name: String,
    table_name: String,
    processing_message: String,

    // Estado de autocompletado
    suggestions: Vec<String>,
    suggestion_index: Option<usize>,
}

impl App {
    fn new() -> App {
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

fn get_path_suggestions(input: &str) -> Vec<String> {
    // Si está vacío, empezamos en la raíz del sistema
    let raw_query = if input.is_empty() { "/" } else { input };
    
    // Soporte para ~
    let query = if raw_query.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            raw_query.replacen('~', &home, 1)
        } else {
            raw_query.to_string()
        }
    } else {
        raw_query.to_string()
    };

    let path = Path::new(&query);
    
    // Determinamos directorio de búsqueda y prefijo
    let (search_dir, prefix) = if query.ends_with(std::path::MAIN_SEPARATOR) {
         (path, "")
    } else {
         if path.parent().is_none() {
             (Path::new("/"), path.to_str().unwrap_or(""))
         } else {
             (path.parent().unwrap(), path.file_name().and_then(|s| s.to_str()).unwrap_or(""))
         }
    };
    
    let search_dir = if search_dir.as_os_str().is_empty() {
        Path::new("/")
    } else {
        search_dir
    };

    let mut suggestions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(search_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Filtrar ocultos y verificar prefijo
            if name.starts_with(prefix) && !name.starts_with('.') {
                let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let is_ndjson = name.ends_with(".ndjson");

                // SOLO mostramos directorios o archivos .ndjson
                if is_dir || is_ndjson {
                    let mut display_path = if raw_query.starts_with('~') {
                        let home = std::env::var("HOME").unwrap_or_default();
                        entry.path().to_string_lossy().replacen(&home, "~", 1)
                    } else {
                        entry.path().to_string_lossy().to_string()
                    };

                    if is_dir {
                        display_path.push(std::path::MAIN_SEPARATOR);
                    }
                    suggestions.push(display_path);
                }
            }
        }
    }
    suggestions.sort();
    suggestions
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub fn run_interactive() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let res = run_app(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;

    if let Err(err) = res { println!("{:?}", err) }
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
                                            app.suggestions = get_path_suggestions(&app.input_buffer);
                                            app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
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
                                        match execute_load_tui(&app.ndjson_path, &app.entity_name, &app.table_name) {
                                            //Ok(_) => app.processing_message = "Carga completada exitosamente!".to_string(),
                                            //Err(e) => app.processing_message = format!("Error: {}", e),
                                            _ => {}
                                        }

                                        app.load_step = LoadStep::Done;
                                    }
                                    LoadStep::Done => {
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
                                             app.suggestions = get_path_suggestions(&app.input_buffer);
                                             app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
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
                                        app.suggestion_index = if app.suggestions.is_empty() { None } else { Some(0) };
                                    },
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

fn ui(f: &mut Frame, app: &mut App, purple: Color) {

    let purple_muted = Color::Rgb(244, 230, 255);

    let size = f.size();



    // 🔹 Layout raíz con margen superior

    let root_layout = Layout::default()

        .direction(Direction::Vertical)

        .constraints([

            Constraint::Length(2), // 👈 margen superior

            Constraint::Min(0),

        ])

        .split(size);



    let content_area = root_layout[1];



    // Layout Principal: Margen de 4 columnas

    let main_layout = Layout::default()

        .direction(Direction::Horizontal)

        .constraints([

            Constraint::Length(4),

            Constraint::Min(0),

            Constraint::Length(4),

        ])

        .split(content_area);



    let central_area = main_layout[1];



    let action_height = 3; 



    let chunks = Layout::default()

        .direction(Direction::Vertical)

        .constraints([

            Constraint::Length(7),             

            Constraint::Length(1),             

            Constraint::Length(action_height), 

            Constraint::Min(0),                

        ])

    .split(central_area);



    // --- 1. RENDERIZAR MENÚ SUPERIOR ---

    let menu_block = Block::default()

        .borders(Borders::ALL)

        .border_type(BorderType::Rounded)

        .border_style(Style::default().fg(purple_muted))

        .padding(Padding::new(2, 2, 1, 1));



    let items: Vec<ListItem> = app.menu_items.iter().enumerate().map(|(i, m)| {

        ListItem::new(format!("{}. {}", i + 1, m))

    }).collect();



    let list = List::new(items)

        .block(menu_block)

        .highlight_style(Style::default().fg(purple))

        .highlight_symbol("◉ ");



    f.render_stateful_widget(list, chunks[0], &mut app.menu_state);



    // --- 2. RENDERIZAR BARRA DE ENTRADA Y ESTADOS ---

    if let Some(_task) = app.active_task {

        match app.load_step {

            LoadStep::Processing => {

                let area = centered_rect(60, 20, f.size());

                f.render_widget(Clear, area);

                f.render_widget(

                    Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" Cargando ").border_style(Style::default().fg(purple)),

                    area

                );

                f.render_widget(

                    Paragraph::new("\nAnalizando y procesando archivos...\nPor favor espera.").alignment(Alignment::Center),

                    area.inner(&Margin { vertical: 1, horizontal: 2 })

                );

                return;

            }

            LoadStep::Done => {

                let area = centered_rect(60, 20, f.size());

                f.render_widget(Clear, area);

                f.render_widget(

                    Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" ¡Éxito! ").border_style(Style::default().fg(Color::Green)),

                    area

                );

                f.render_widget(

                    Paragraph::new("\nDatos cargados correctamente.\n\nPresiona [Enter] para continuar.").alignment(Alignment::Center),

                    area.inner(&Margin { vertical: 1, horizontal: 2 })

                );

                return;

            }

            _ => {}

        }



        let input_block = Block::default()

            .borders(Borders::ALL)

            .border_type(BorderType::Rounded)

            .border_style(Style::default().fg(purple_muted));



        f.render_widget(&input_block, chunks[2]);



                let inner_area = input_block.inner(chunks[2]);



                let input_area = inner_area.inner(&Margin {



                    vertical: 0,



                    horizontal: 1,



                });



        



        let centered_input_area = Rect {

            x: input_area.x,

            y: input_area.y + (input_area.height / 2),

            width: input_area.width,

            height: 1,

        };



                let placeholder = match app.load_step {



                    LoadStep::InputPath => "Browse or type file path (ends in .ndjson)...",



                    LoadStep::InputEntity => "Nombre de la entidad (ej: users)",



                    LoadStep::InputTable => "Nombre de la tabla (ej: main)",



                    _ => "",



                };



        



        let prompt_str = " > ";

        

        let mut spans = vec![

            Span::styled(prompt_str, Style::default().fg(purple).add_modifier(Modifier::BOLD)),

        ];



        if app.input_buffer.is_empty() {

            spans.push(Span::styled(" ", Style::default().bg(Color::White)));

            spans.push(Span::raw(" "));

            spans.push(Span::styled(placeholder, Style::default().fg(Color::DarkGray)));

        } else {

            spans.push(Span::raw(" "));

            spans.push(Span::raw(&app.input_buffer));

        }



        f.render_widget(Paragraph::new(Line::from(spans)), centered_input_area);



        if !app.input_buffer.is_empty() {

            let cursor_x = centered_input_area.x 

                + prompt_str.chars().count() as u16 

                + 1 

                + app.input_buffer.chars().count() as u16;

            f.set_cursor(cursor_x, centered_input_area.y);

        }

        

                // --- 3. RENDERIZAR SUGERENCIAS ---

        

                if !app.suggestions.is_empty() {

        

                    let suggestions_items: Vec<ListItem> = app.suggestions.iter().map(|m| {

        

                        ListItem::new(Span::styled(format!("  {}", m), Style::default().fg(Color::DarkGray)))

        

                    }).collect();

        

        

        

                    let mut suggestion_state = ListState::default();

        

                    suggestion_state.select(app.suggestion_index);

        

                    

        

                    let list_widget = List::new(suggestions_items)

        

                        .highlight_style(Style::default().fg(purple).add_modifier(Modifier::BOLD))

        

                        .highlight_symbol("  > ");

        

                        

        

                    f.render_stateful_widget(list_widget, chunks[3], &mut suggestion_state);
                }
            }
        }

        

        



        

        
