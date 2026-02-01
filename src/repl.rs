use std::io;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::time::Duration;
use crate::commands::load::execute_load_tui;

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
                                match app.load_step {
                                    LoadStep::InputPath => {
                                        app.ndjson_path = app.input_buffer.clone();
                                        app.input_buffer.clear();
                                        app.load_step = LoadStep::InputEntity;
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
                            KeyCode::Char(c) => app.input_buffer.push(c),
                            KeyCode::Backspace => { app.input_buffer.pop(); }
                            KeyCode::Esc => { 
                                app.active_task = None; 
                                app.input_buffer.clear();
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
                                    Some(0) => app.active_task = Some("Load"),
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

    let action_height = 3; // Altura fija y mínima para un input tipo "barra"

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),             // Menú (alto fijo)
            Constraint::Length(1),             // Espacio pequeño entre menú e input
            Constraint::Length(action_height), // Barra de input (3 líneas)
            Constraint::Min(0),                // Todo el espacio sobrante queda abajo
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

    // --- 2. RENDERIZAR BARRA DE ENTRADA (Minimalista) ---
    if let Some(_task) = app.active_task {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted));

        f.render_widget(&input_block, chunks[2]);

        let inner_area = input_block.inner(chunks[2]);
        let input_area = inner_area.inner(&layout::Margin {
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
            LoadStep::InputPath => "path/to/file.ndjson",
            LoadStep::InputEntity => "entity name",
            LoadStep::InputTable => "table name",
            _ => "unknown step",
        };

        let prompt_str = " > ";
        
        // 1. CONSTRUCCIÓN VISUAL
        let mut spans = vec![
            Span::styled(prompt_str, Style::default().fg(purple).add_modifier(Modifier::BOLD)),
        ];

        if app.input_buffer.is_empty() {
            // DIBUJAMOS EL CURSOR BLANCO MANUALMENTE
            spans.push(Span::styled(" ", Style::default().bg(Color::White)));
            // Espacio de separación y placeholder
            spans.push(Span::raw(" "));
            spans.push(Span::styled(placeholder, Style::default().fg(Color::DarkGray)));
        } else {
            // Si hay texto, espacio de respiro y el texto del usuario
            spans.push(Span::raw(" "));
            spans.push(Span::raw(&app.input_buffer));
        }

        f.render_widget(Paragraph::new(Line::from(spans)), centered_input_area);

        // 2. GESTIÓN DEL CURSOR DEL SISTEMA (LA BARRITA AZUL)
        if matches!(
            app.load_step,
            LoadStep::InputPath | LoadStep::InputEntity | LoadStep::InputTable
        ) {
            if !app.input_buffer.is_empty() {
                // SOLO mostramos el cursor real cuando el usuario empieza a escribir
                // así evitamos que el cuadro azul tape nuestro bloque blanco inicial
                let cursor_x = centered_input_area.x 
                    + prompt_str.chars().count() as u16 
                    + 1 
                    + app.input_buffer.chars().count() as u16;
                
                f.set_cursor(cursor_x, centered_input_area.y);
            } else {
                // OPCIONAL: Si quieres que el cursor "exista" pero no tape el bloque, 
                // puedes mandarlo a una esquina oculta, o simplemente no llamar a set_cursor.
                // Al no llamarlo, se verá el bloque blanco que dibujamos arriba.
            }
        }
    }
}