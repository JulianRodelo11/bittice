use std::io;
use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{prelude::*, widgets::*};
use std::time::Duration;

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

    // Dividir el área central en: Menú (Arriba) y Acción (Abajo)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Altura fija para el menú
            Constraint::Length(3), // Altura fija para el separador
            Constraint::Min(0),    // El resto para la acción
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

    // --- 2. RENDERIZAR CONTENEDOR DE ACCIÓN ---
    if let Some(_task) = app.active_task {
        let action_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted))
            .padding(Padding::new(2, 2, 0, 0));

        let action_inner = action_block.inner(chunks[1]);
        f.render_widget(action_block, chunks[1]);

        // Texto tipo input
        let input = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("> ", Style::default().fg(purple)),
                Span::styled(
                    "File path .ndjson",
                    Style::default().fg(Color::DarkGray),
                )
            ])
        ])
        .wrap(Wrap { trim: true });

        f.render_widget(input, action_inner);
    }
}