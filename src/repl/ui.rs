use crate::repl::state::{App, FocusPanel, SearchCriteria, FilterStep, LoadStep};
use crate::repl::utils::get_loaded_data;
use ratatui::{prelude::*, widgets::*};

pub fn ui(f: &mut Frame, app: &mut App) {
    let purple = Color::Rgb(197, 137, 249);
    let purple_muted = Color::Rgb(244, 230, 255);
    
    let size = f.size();

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(0)]) // Margen superior
        .split(size);
    
    let content_area = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(4), Constraint::Min(0), Constraint::Length(4)]) // Márgenes laterales
        .split(chunks[1])[1];

    if app.active_task == Some("Search") {
        draw_search_ui(f, app, content_area);
    } else if app.active_task == Some("Load") {
        draw_load_ui(f, app, content_area, purple, purple_muted);
    } else {
        draw_main_menu(f, app, content_area, purple, purple_muted);
    }
}

fn draw_load_ui(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let top_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3)])
        .split(chunks[0]);

    let (prompt_text, placeholder) = match app.load_step {
        LoadStep::InputPath => ("Enter path to .ndjson file:", " /path/to/your/file.ndjson"),
        LoadStep::InputEntity => ("Enter entity name:", " (e.g., customers, products)"),
        LoadStep::InputTable => ("Enter table name:", " (e.g., 2024_sales, user_profiles)"),
        _ => ("", ""),
    };

    let prompt_block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(purple))
        .padding(Padding::new(2, 2, 1, 1));
    
    let prompt_paragraph = Paragraph::new(prompt_text).block(prompt_block);
    f.render_widget(prompt_paragraph, top_chunks[0]);
    
    let input_text: Vec<Span> = vec![
        Span::raw(&app.input_buffer),
        Span::styled(placeholder, Style::default().fg(Color::DarkGray)),
    ];
    let input_line = Line::from(input_text);

    let input_paragraph = Paragraph::new(input_line)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted))
            .padding(Padding::new(2, 2, 1, 1)));
    f.render_widget(input_paragraph, top_chunks[1]);
    f.set_cursor(top_chunks[1].x + 3 + app.input_buffer.len() as u16, top_chunks[1].y + 2);

    if !app.suggestions.is_empty() {
        let mut suggestion_state = ListState::default();
        suggestion_state.select(app.suggestion_index);
        
        let suggestion_items: Vec<ListItem> = app.suggestions.iter().map(|s| ListItem::new(s.as_str())).collect();
        let list = List::new(suggestion_items)
            .block(Block::default().padding(Padding::new(0, 0, 1, 0)))
            .highlight_style(Style::default().bg(purple).fg(Color::Black));
        f.render_stateful_widget(list, chunks[1], &mut suggestion_state);
    }
}

fn draw_main_menu(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let menu_block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .border_style(Style::default().fg(purple_muted))
        .padding(Padding::new(2, 2, 1, 1));

    let items: Vec<ListItem> = app.menu_items.iter().map(|i| ListItem::new(*i)).collect();
    let list = List::new(items)
        .block(menu_block)
        .highlight_style(Style::default().fg(purple))
        .highlight_symbol("◉ ");
    f.render_stateful_widget(list, chunks[0], &mut app.menu_state);

    let loaded_data = get_loaded_data();
    if !loaded_data.is_empty() {
        let loaded_items: Vec<ListItem> = loaded_data.iter().map(|s| ListItem::new(s.as_str())).collect();
        let list = List::new(loaded_items).block(Block::default().padding(Padding::new(0, 0, 1, 0)));
        f.render_widget(list, chunks[1]);
    }
}

fn draw_search_ui(f: &mut Frame, app: &mut App, area: Rect) {
    let active_color = Color::Rgb(128, 222, 152);
    let inactive_color = Color::Rgb(244, 230, 255);//

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
    
    // Altura exacta: items + padding vertical (2) + bordes (2)
    let content_height = (left_len as u16 + 4).max(middle_len as u16 + 4).max(right_len as u16 + 4);

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Menu superior
            Constraint::Length(content_height), // Paneles de búsqueda
            Constraint::Min(0), // Espacio restante (para simetría/balance)
            Constraint::Length(if app.focus_panel == FocusPanel::Bottom { 3 } else { 0 }), // Input inferior
        ])
        .split(area);
    
    draw_main_menu(f, app, main_chunks[0], active_color, inactive_color);
    
    let panel_layout = if app.search_criteria == SearchCriteria::Filters {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(25), Constraint::Percentage(25), Constraint::Percentage(50)])
            .split(main_chunks[1])
    } else {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(main_chunks[1])
    };

    let create_panel = |focus: bool| {
        Block::default().borders(Borders::ALL).border_type(BorderType::Rounded)
            .padding(Padding::new(2, 2, 1, 1))
            .border_style(Style::default().fg(if focus { active_color } else { inactive_color }))
    };

    // --- Panel Izquierdo ---
    let left_items = vec![ListItem::new("Entity"), ListItem::new("Table"), ListItem::new("Filters")];
    let left_list = List::new(left_items)
        .block(create_panel(app.focus_panel == FocusPanel::Left))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("◉ ");
    f.render_stateful_widget(left_list, panel_layout[0], &mut app.left_panel_state);

    // --- Panel Medio ---
    let middle_items: Vec<ListItem> = match app.search_criteria {
        SearchCriteria::Entity => app.search_entities.iter().map(|s| ListItem::new(s.as_str())).collect(),
        SearchCriteria::Table => app.search_tables.iter().map(|s| ListItem::new(s.as_str())).collect(),
        SearchCriteria::Filters => vec![ListItem::new("Field"), ListItem::new("Op"), ListItem::new("Value")],
    };
    let middle_list = List::new(middle_items)
        .block(create_panel(app.focus_panel == FocusPanel::Middle))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("◉ ");
    f.render_stateful_widget(middle_list, panel_layout[1], &mut app.middle_panel_state);

    // --- Panel Derecho (Solo Filtros) ---
    if app.search_criteria == SearchCriteria::Filters {
        let right_items: Vec<ListItem> = match app.filter_step {
            FilterStep::Field => app.available_fields.iter().map(|s| ListItem::new(s.as_str())).collect(),
            FilterStep::Op => vec![ListItem::new("Eq")],
            FilterStep::Value => vec![ListItem::new(if app.filter_value_input.is_empty() { "Press Enter to type..." } else { app.filter_value_input.as_str() })],
        };
        let right_list = List::new(right_items)
            .block(create_panel(app.focus_panel == FocusPanel::Right))
            .highlight_style(Style::default().fg(active_color))
            .highlight_symbol("◉ ");
        f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);
    }
    
    // --- Input Inferior ---
    if app.focus_panel == FocusPanel::Bottom {
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(active_color))
            .padding(Padding::new(2, 2, 0, 0)); // Padding para alinear texto
        let p = Paragraph::new(app.filter_value_input.as_str()).block(input_block);
        f.render_widget(p, main_chunks[3]);
        f.set_cursor(main_chunks[3].x + 3 + app.filter_value_input.len() as u16, main_chunks[3].y + 1);
    }
}