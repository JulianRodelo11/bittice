use crate::repl::state::{App, FocusPanel, SearchCriteria, FilterStep, LoadStep};
use crate::repl::utils::get_loaded_data;
use ratatui::layout::Margin;
use ratatui::{prelude::*, widgets::*};

pub fn ui(f: &mut Frame, app: &mut App, purple: Color) {
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

    if app.active_task == Some("Search") {
        draw_search_ui(f, app, central_area);
    } else if app.active_task == Some("Load") {
        draw_load_ui(f, app, central_area, purple, purple_muted);
    } else {
        draw_main_menu(f, app, central_area, purple, purple_muted);
    }
}

fn draw_load_ui(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let loaded_data = get_loaded_data();
    let loaded_height = if loaded_data.is_empty() {
        0
    } else {
        (loaded_data.len() as u16).min(8) + 1
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Menú
            Constraint::Length(loaded_height), // Datos cargados
            Constraint::Length(1), // Separador
            Constraint::Length(3), // Input
            Constraint::Min(0),    // Sugerencias
        ])
        .split(area);

    // 1. Menú (Se dibuja SIEMPRE)
    draw_menu_widget(f, app, chunks[0], purple, purple_muted);

    // 2. Datos Cargados (Se dibujan SIEMPRE)
    if !loaded_data.is_empty() {
        draw_loaded_data_widget(f, &loaded_data, chunks[1]);
    }

    // Si estamos procesando, nos detenemos aquí para dejar espacio al spinner de terminal
    if app.load_step == LoadStep::Processing {
        return;
    }

    // 3. Input (Solo si no estamos procesando)
    let placeholder = match app.load_step {
        LoadStep::InputPath => "Browse or type file path (ends in .ndjson)...",
        LoadStep::InputEntity => "Entity name (e.g. users)",
        LoadStep::InputTable => "Table name (e.g. main)",
        _ => "",
    };
    draw_input_widget(f, app, chunks[3], placeholder, purple, purple_muted);

    // 4. Sugerencias
    if !app.suggestions.is_empty() {
        draw_suggestions_widget(f, app, chunks[4], purple);
    }
}

fn draw_search_ui(f: &mut Frame, app: &mut App, area: Rect) {
    let purple = Color::Rgb(197, 137, 249);
    let purple_muted = Color::Rgb(244, 230, 255);
    let active_color = Color::Rgb(137, 180, 249);
    let inactive_color = Color::Rgb(244, 230, 255);

    let left_len = 3;
    let middle_len = match app.search_criteria {
        SearchCriteria::Entity => app.search_entities.len(),
        SearchCriteria::Table => app.search_tables.len(),
        SearchCriteria::Filters => 3,
    };
    let right_len = if app.search_criteria == SearchCriteria::Filters {
        match app.filter_step {
            FilterStep::Field => app.available_fields.len(),
            _ => 1,
        }
    } else { 0 };
    
    let content_height = (left_len as u16 + 4).max(middle_len as u16 + 4).max(right_len as u16 + 4);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Menú
            Constraint::Length(content_height), // Paneles
            Constraint::Length(if app.focus_panel == FocusPanel::Bottom { 3 } else { 0 }), // Input inferior
            Constraint::Length(1), // Instrucciones justo debajo
            Constraint::Min(0),    // Resto del espacio al final
        ])
        .split(area);
    
    // Aquí mantenemos los colores PÚRPURA para el menú superior
    draw_menu_widget(f, app, chunks[0], purple, purple_muted);
    
    let panel_layout = if app.search_criteria == SearchCriteria::Filters {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(25), Constraint::Percentage(25), Constraint::Percentage(50)])
            .split(chunks[1])
    } else {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1])
    };

    let create_panel = |focus: bool| {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(2, 2, 1, 1))
            .border_style(Style::default().fg(if focus { active_color } else { inactive_color }))
    };

    // Panel Izquierdo
    let left_items = vec![
        ListItem::new("Entity"),
        ListItem::new(Span::styled(
            "Table",
            if app.selected_entity.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
        ListItem::new(Span::styled(
            "Filters",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
    ];
    let left_list = List::new(left_items)
        .block(create_panel(app.focus_panel == FocusPanel::Left))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(left_list, panel_layout[0], &mut app.left_panel_state);

    // Panel Medio
    let middle_items: Vec<ListItem> = match app.search_criteria {
        SearchCriteria::Entity => app.search_entities.iter().map(|s| {
            let is_selected = Some(s.clone()) == app.selected_entity;
            let circle = if is_selected {
                Span::styled("◉", Style::default().fg(active_color))
            } else {
                Span::raw("○")
            };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
        }).collect(),
        SearchCriteria::Table => app.search_tables.iter().map(|s| {
            let is_selected = Some(s.clone()) == app.selected_table;
            let circle = if is_selected {
                Span::styled("◉", Style::default().fg(active_color))
            } else {
                Span::raw("○")
            };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
        }).collect(),
        SearchCriteria::Filters => vec![
            ListItem::new("Field"),
            ListItem::new("Op"),
            ListItem::new("Value"),
        ],
    };
    let mut middle_list = List::new(middle_items)
        .block(create_panel(app.focus_panel == FocusPanel::Middle))
        .highlight_style(Style::default().fg(active_color));
    
    if app.search_criteria == SearchCriteria::Filters {
        middle_list = middle_list.highlight_symbol("> ");
    }
    
    f.render_stateful_widget(middle_list, panel_layout[1], &mut app.middle_panel_state);

    // Panel Derecho
    if app.search_criteria == SearchCriteria::Filters {
        let right_items: Vec<ListItem> = match app.filter_step {
            FilterStep::Field => app.available_fields.iter().map(|s| {
                let is_selected = Some(s.clone()) == app.selected_field;
                let circle = if is_selected {
                    Span::styled("◉", Style::default().fg(active_color))
                } else {
                    Span::raw("○")
                };
                ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
            }).collect(),
            FilterStep::Op => vec![
                ListItem::new(Line::from(vec![
                    Span::styled("◉", Style::default().fg(active_color)),
                    Span::raw(format!(" {}", app.selected_op))
                ]))
            ],
            FilterStep::Value => vec![ListItem::new(if app.filter_value_input.is_empty() { "Press Enter to type..." } else { app.filter_value_input.as_str() })],
        };
        let right_list = List::new(right_items)
            .block(create_panel(app.focus_panel == FocusPanel::Right))
            .highlight_style(Style::default().fg(active_color));
        f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);
    }
    
    // Input Inferior
    if app.focus_panel == FocusPanel::Bottom {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(active_color))
            .padding(Padding::new(2, 2, 0, 0));
        let p = Paragraph::new(app.filter_value_input.as_str()).block(input_block);
        f.render_widget(p, chunks[3]);
        f.set_cursor(chunks[3].x + 3 + app.filter_value_input.len() as u16, chunks[3].y + 1);
    }

    // Instrucciones minimalistas con descripciones claras
    let desc_style = Style::default().fg(purple);
    let separator = "  •  ";

    let help_text = match app.focus_panel {
        FocusPanel::Left => format!("↑↓ Navigate{}→ Next Panel{}esq Quit", separator, separator),
        FocusPanel::Middle => format!("↑↓ Navigate{}↵ Toggle Selection{}←→ Switch Panel{}esq Quit", separator, separator, separator),
        FocusPanel::Right => format!("↑↓ Navigate{}↵ Toggle Selection{}← Prev Panel{}esq Quit", separator, separator, separator),
        FocusPanel::Bottom => format!("↵ Accept{}esq Cancel", separator),
    };

    let instructions_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)]) 
        .split(chunks[3]);

    f.render_widget(
        Paragraph::new(help_text).style(desc_style).alignment(Alignment::Right),
        instructions_layout[0]
    );
}

fn draw_main_menu(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    draw_menu_widget(f, app, chunks[0], purple, purple_muted);

    let loaded_data = get_loaded_data();
    if !loaded_data.is_empty() {
        draw_loaded_data_widget(f, &loaded_data, chunks[1]);
    }
}

// --- Helpers de UI ---

fn draw_menu_widget(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let menu_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(purple_muted))
        .padding(Padding::new(2, 2, 1, 1));

    let items: Vec<ListItem> = app.menu_items.iter().enumerate()
        .map(|(i, m)| ListItem::new(format!("{}. {}", i + 1, m)))
        .collect();

    let list = List::new(items)
        .block(menu_block)
        .highlight_style(Style::default().fg(purple))
        .highlight_symbol("◉ ");

    f.render_stateful_widget(list, area, &mut app.menu_state);
}

fn draw_loaded_data_widget(f: &mut Frame, data: &[String], area: Rect) {
    let items: Vec<ListItem> = data.iter()
        .map(|s| ListItem::new(Span::styled(s, Style::default().fg(Color::DarkGray))))
        .collect();
    let list = List::new(items).block(Block::default().padding(Padding::new(0, 0, 1, 0)));
    f.render_widget(list, area);
}

fn draw_input_widget(f: &mut Frame, app: &mut App, area: Rect, placeholder: &str, purple: Color, purple_muted: Color) {
    let input_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(purple_muted));
    f.render_widget(&input_block, area);

    let inner = input_block.inner(area).inner(&Margin { vertical: 0, horizontal: 1 });
    let centered_area = Rect { x: inner.x, y: inner.y + (inner.height / 2), width: inner.width, height: 1 };

    let prompt_str = " > ";
    let mut spans = vec![Span::styled(prompt_str, Style::default().fg(purple).add_modifier(Modifier::BOLD))];

    if app.input_buffer.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(placeholder, Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(&app.input_buffer));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), centered_area);
    if !app.input_buffer.is_empty() {
        f.set_cursor(centered_area.x + prompt_str.chars().count() as u16 + 1 + app.input_buffer.chars().count() as u16, centered_area.y);
    }
}

fn draw_suggestions_widget(f: &mut Frame, app: &mut App, area: Rect, purple: Color) {
    let items: Vec<ListItem> = app.suggestions.iter()
        .map(|m| ListItem::new(Span::styled(m.as_str(), Style::default().fg(Color::DarkGray))))
        .collect();
    let mut state = ListState::default();
    state.select(app.suggestion_index);
    let list = List::new(items)
        .highlight_style(Style::default().fg(purple).add_modifier(Modifier::BOLD))
        .highlight_symbol("   > ");
    f.render_stateful_widget(list, area, &mut state);
}
