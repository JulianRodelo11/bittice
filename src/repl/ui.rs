use crate::repl::state::{App, FocusPanel, SearchCriteria, FilterStep, LoadStep};
use crate::repl::utils::{get_loaded_data, get_order_by_fields, get_filtered_fields, get_base_fields};
use crate::ui::colors;
use ratatui::layout::Margin;
use ratatui::{prelude::*, widgets::*};

pub fn ui(f: &mut Frame, app: &mut App, _purple: Color) {
    let _purple_muted = colors::MUTED_COLOR;
    let size = f.size();

    // 🔹 Layout raíz con simetría vertical
    let root_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // Margen superior
            Constraint::Min(0),
            Constraint::Length(2), // Margen inferior (igual al superior)
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

    let is_overlay_active = app.is_saving_query || app.show_saved_queries;

    if app.active_task == Some("Search") {
        draw_search_ui(f, app, central_area, is_overlay_active);
        draw_overlays(f, app, size);
    } else if app.active_task == Some("Load") {
        draw_load_ui(f, app, central_area, is_overlay_active);
    } else if app.active_task == Some("Server") {
        draw_server_ui(f, app, central_area, is_overlay_active);
    } else {
        draw_main_menu(f, app, central_area, is_overlay_active);
    }
}

fn draw_server_ui(f: &mut Frame, app: &mut App, area: Rect, dimmed: bool) {
    let purple = if dimmed { Color::Indexed(237) } else { colors::PRIMARY_COLOR };
    let purple_muted = if dimmed { Color::Indexed(235) } else { colors::MUTED_COLOR };
    let text_color = if dimmed { Color::Indexed(237) } else { Color::White };
    let log_color = if dimmed { Color::Indexed(240) } else { colors::GREEN };
    let active_color = if dimmed { Color::Indexed(243) } else { colors::ACTIVE_COLOR };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // Menu
            Constraint::Length(3), // Status Header
            Constraint::Min(0),    // Main Content (Split)
            Constraint::Length(1), // Footer
        ])
        .split(area);

    // 1. Menu
    draw_menu_widget(f, app, chunks[0], purple, purple_muted, text_color);

    // 2. Status Header
    let status_text = if app.is_server_running {
        Span::styled(" ● Server Running on http://127.0.0.1:3000 ", Style::default().fg(colors::GREEN).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" ● Server Stopped ", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    };
    
    let status_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(purple_muted));
    
    f.render_widget(Paragraph::new(status_text).block(status_block).alignment(Alignment::Center), chunks[1]);

    // 3. Main Content
    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40), // Available Endpoints
            Constraint::Percentage(60), // Logs
        ])
        .split(chunks[2]);

    // Left: Available Endpoints
    let queries = crate::core::saved_queries::load_queries().unwrap_or_default();
    let items: Vec<ListItem> = queries.iter().map(|q| {
        let mut params = Vec::new();
        
        // Extract params from filters
        for f in &q.filters {
            if f.value.starts_with('$') {
                params.push(f.value[1..].to_string());
            }
        }
        
        // Extract params from aggregations
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
        
        params.sort();
        params.dedup();
        
        let mut path = format!("/{}", q.name);
        if !params.is_empty() {
            let query_string = params.iter().map(|p| format!("{}=?", p)).collect::<Vec<_>>().join("&");
            path.push('?');
            path.push_str(&query_string);
        }

        ListItem::new(Line::from(vec![
            Span::styled("GET ", Style::default().fg(colors::ACTIVE_COLOR).add_modifier(Modifier::BOLD)),
            Span::styled(path, Style::default().fg(text_color)),
        ]))
    }).collect();

    let endpoints_focus = app.server_focus == crate::repl::state::ServerFocus::Endpoints;
    let endpoints_list = List::new(items)
        .block(Block::default()
            .title(" Available Endpoints ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::uniform(1))
            .border_style(Style::default().fg(if endpoints_focus { active_color } else { purple_muted })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol(if endpoints_focus { "> " } else { "  " });
    f.render_stateful_widget(endpoints_list, main_chunks[0], &mut app.endpoint_state);

    // Right: Logs
    let log_items: Vec<ListItem> = app.server_logs.iter().rev().map(|log| {
        ListItem::new(Span::styled(log, Style::default().fg(log_color)))
    }).collect();

    let logs_focus = app.server_focus == crate::repl::state::ServerFocus::Logs;
    let logs_list = List::new(log_items)
        .block(Block::default()
            .title(" Server Logs ")
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::uniform(1))
            .border_style(Style::default().fg(if logs_focus { active_color } else { purple_muted })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol(if logs_focus { "> " } else { "  " });
    f.render_stateful_widget(logs_list, main_chunks[1], &mut app.log_state);

    // 4. Footer
    let footer_text = "Esc: Back • Tab: Switch Panel • ↑↓: Navigate • c: Copy to Clipboard";
    f.render_widget(Paragraph::new(footer_text).style(Style::default().fg(Color::DarkGray)).alignment(Alignment::Center), chunks[3]);
}

fn draw_overlays(f: &mut Frame, app: &mut App, area: Rect) {
    if app.is_saving_query {
        draw_save_query_overlay(f, app, area);
    }
    if app.show_saved_queries {
        draw_saved_queries_overlay(f, app, area);
    }
    if app.is_prompting_variable {
        draw_variable_prompt_overlay(f, app, area);
    }
}

fn draw_load_ui(f: &mut Frame, app: &mut App, area: Rect, dimmed: bool) {
    let purple = if dimmed { Color::Indexed(237) } else { colors::PRIMARY_COLOR };
    let purple_muted = if dimmed { Color::Indexed(235) } else { colors::MUTED_COLOR };
    let text_color = if dimmed { Color::Indexed(237) } else { Color::White };

    let loaded_data = get_loaded_data();
    let loaded_height = if loaded_data.is_empty() {
        0
    } else {
        (loaded_data.len() as u16).min(8) + 1
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // Menú
            Constraint::Length(loaded_height), // Datos cargados
            Constraint::Length(1), // Separador
            Constraint::Length(3), // Input
            Constraint::Min(0),    // Sugerencias
        ])
        .split(area);

    // 1. Menú (Se dibuja SIEMPRE)
    draw_menu_widget(f, app, chunks[0], purple, purple_muted, text_color);

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

fn draw_search_ui(f: &mut Frame, app: &mut App, area: Rect, dimmed: bool) {
    let purple = if dimmed { Color::Indexed(240) } else { colors::PRIMARY_COLOR };
    let purple_muted = if dimmed { Color::Indexed(238) } else { colors::MUTED_COLOR };
    let active_color = if dimmed { Color::Indexed(243) } else { colors::ACTIVE_COLOR };
    let inactive_color = if dimmed { Color::Indexed(238) } else { colors::INACTIVE_COLOR };
    let value_color = if dimmed { Color::Indexed(240) } else { colors::VALUE_COLOR };
    let key_color = if dimmed { Color::Indexed(240) } else { colors::KEY_COLOR };
    let instruction_color = if dimmed { Color::Indexed(240) } else { colors::INSTRUCTION_COLOR };
    let text_color = if dimmed { Color::Indexed(240) } else { Color::White };
    let grid_color = if dimmed { Color::Indexed(237) } else { Color::Rgb(100, 100, 100) };

    // --- QUERY RESULTS MODE (GLOBAL SCROLL) ---
    if let Some(results) = &app.search_results {
        let results_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(0),    // Main Scrollable Area (Menu + Query + Table)
                Constraint::Length(1), // Fixed Footer (Single line)
            ])
            .split(area);

        // 1. Build All Scrollable Content
        let mut all_lines = Vec::new();
        
        // Menu
        all_lines.extend(get_menu_lines(app, purple, purple_muted, text_color));
        all_lines.push(Line::from(""));

        // Exit Instructions (Normal text above Query)
        all_lines.push(Line::from(vec![
            Span::styled("+ ", Style::default().fg(active_color).add_modifier(Modifier::BOLD)),
            Span::styled("Press Esc to exit results", Style::default().fg(text_color)),
        ]));
        all_lines.push(Line::from(vec![
            Span::styled("+ ", Style::default().fg(active_color).add_modifier(Modifier::BOLD)),
            Span::styled("Press 'S' (Shift+s) to save query", Style::default().fg(text_color)),
        ]));
        all_lines.push(Line::from(""));
        
        // Query Preview
        all_lines.extend(get_query_preview_lines_styled(app, key_color, value_color, active_color));
        all_lines.push(Line::from(""));

        // Execution Time
        let time_str = if results.execution_time_micros < 1000 {
            format!("{} µs", results.execution_time_micros)
        } else if results.execution_time_micros < 1_000_000 {
            format!("{:.2} ms", results.execution_time_micros as f64 / 1000.0)
        } else {
            format!("{:.2} s", results.execution_time_micros as f64 / 1_000_000.0)
        };

        all_lines.push(Line::from(vec![
            Span::styled("Time: ", Style::default().fg(key_color)),
            Span::styled(time_str, Style::default().fg(value_color)),
        ]));
        all_lines.push(Line::from(""));

        // Table Header & Rows
        if results.rows.is_empty() {
             all_lines.push(Line::from(Span::styled("  No results found", Style::default().fg(if dimmed { Color::Indexed(235) } else { Color::Red }))));
        } else {
            let mut col_widths = Vec::new();
            for (i, header) in results.headers.iter().enumerate() {
                let mut max_w = header.len();
                for row in &results.rows {
                    if let Some(val) = row.get(i) {
                        if val.len() > max_w { max_w = val.len(); }
                    }
                }
                max_w = max_w.min(40);
                col_widths.push(max_w + 2);
            }

            let num_cols = results.headers.len();
            if num_cols > 0 {
                let mut top = String::from("┌");
                for (i, &w) in col_widths.iter().enumerate() {
                    top.push_str(&"─".repeat(w));
                    if i < num_cols - 1 { top.push('┬'); }
                }
                top.push('┐');
                all_lines.push(Line::from(Span::styled(top, Style::default().fg(grid_color))));

                let mut header_line = Vec::new();
                header_line.push(Span::styled("│", Style::default().fg(grid_color)));
                for (i, h) in results.headers.iter().enumerate() {
                    let w = col_widths[i];
                    let truncated = if h.len() > w - 2 { &h[..w - 2] } else { h };
                    let cell = format!(" {:<width$} ", truncated, width = w - 2);
                    header_line.push(Span::styled(cell, Style::default().fg(active_color)));
                    header_line.push(Span::styled("│", Style::default().fg(grid_color)));
                }
                all_lines.push(Line::from(header_line));

                let mut middle = String::from("├");
                for (i, &w) in col_widths.iter().enumerate() {
                    middle.push_str(&"─".repeat(w));
                    if i < num_cols - 1 { middle.push('┼'); }
                }
                middle.push('┤');
                all_lines.push(Line::from(Span::styled(middle.clone(), Style::default().fg(grid_color))));

                for (r_idx, row) in results.rows.iter().enumerate() {
                    let mut row_line = Vec::new();
                    row_line.push(Span::styled("│", Style::default().fg(grid_color)));
                    for (i, cell_val) in row.iter().enumerate() {
                        let w = col_widths[i];
                        let truncated = if cell_val.len() > w - 2 { &cell_val[..w - 2] } else { cell_val };
                        let cell = format!(" {:<width$} ", truncated, width = w - 2);
                        row_line.push(Span::styled(cell, Style::default().fg(value_color)));
                        row_line.push(Span::styled("│", Style::default().fg(grid_color)));
                    }
                    all_lines.push(Line::from(row_line));

                    if r_idx < results.rows.len() - 1 {
                        all_lines.push(Line::from(Span::styled(middle.clone(), Style::default().fg(grid_color))));
                    } else {
                        let mut bottom = String::from("└");
                        for (i, &w) in col_widths.iter().enumerate() {
                            bottom.push_str(&"─".repeat(w));
                            if i < num_cols - 1 { bottom.push('┴'); }
                        }
                        bottom.push('┘');
                        all_lines.push(Line::from(Span::styled(bottom, Style::default().fg(grid_color))));
                    }
                }
            }
        }
        
        // Render scrollable content
        let content_height = all_lines.len() as u16;
        let viewport_height = results_layout[0].height;
        f.render_widget(Paragraph::new(all_lines).scroll((app.results_scroll, app.results_scroll_x)), results_layout[0]);
        app.last_rendered_content_height = content_height;
        app.results_viewport_height = viewport_height;

        // 2. Build Fixed Footer (Aligned to start/left)
        let limit = app.limit.unwrap_or(100).max(1);
        let total_pages = results.total_found.div_ceil(limit);
        
        let footer_color = if dimmed { Color::Indexed(240) } else { Color::White };
        let footer_bg = if dimmed { Color::Indexed(235) } else { Color::Rgb(30, 30, 30) };
        let footer_muted = if dimmed { Color::Indexed(238) } else { Color::DarkGray };

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ← A ", Style::default().fg(if app.results_page > 1 { footer_color } else { footer_muted }).bg(footer_bg)),
                Span::styled(format!(" Page {}/{} ", app.results_page, total_pages.max(1)), Style::default().fg(footer_color)),
                Span::styled(" D → ", Style::default().fg(if app.results_page < total_pages { footer_color } else { footer_muted }).bg(footer_bg)),
                Span::styled(format!("  (Total found: {})", results.total_found), Style::default().fg(footer_muted)),
            ])).alignment(Alignment::Left),
            results_layout[1]
        );
        return;
    }

    let content_height = if app.filters.len() > 1 { 12 } else { 11 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8), // Menú
            Constraint::Length(content_height), // Paneles
            Constraint::Length(1), // Instrucciones justo debajo
            Constraint::Length(if app.focus_panel == FocusPanel::Bottom { 3 } else { 0 }), // Input inferior
            Constraint::Length(1), // Espacio de separación
            Constraint::Length(3), // Nueva instrucción: Para hacer una consulta... (Aumentado a 3)
            Constraint::Length(1), // Espacio extra solicitado
            Constraint::Length(15), // Preview de Query
            Constraint::Min(0),    // Resto del espacio al final
        ])
        .split(area);
    
    draw_menu_widget(f, app, chunks[0], purple, purple_muted, text_color);

    let panel_layout = if app.search_criteria == SearchCriteria::Filters {
        if app.filters.is_empty() {
            Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(chunks[1])
        } else {
            Layout::default().direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(40)
                ])
                .split(chunks[1])
        }
    } else if app.search_criteria == SearchCriteria::Aggregations {
        if app.aggregations.is_empty() {
            Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(chunks[1])
        } else {
            Layout::default().direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(40)
                ])
                .split(chunks[1])
        }
    } else if app.search_criteria == SearchCriteria::OrderBy {
        if app.order_by.is_empty() {
            Layout::default().direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
                .split(chunks[1])
        } else {
            Layout::default().direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(20), 
                    Constraint::Percentage(40)
                ])
                .split(chunks[1])
        }
    } else {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1])
    };

    let mut left_items = vec![
        ListItem::new(Span::styled("Entity", Style::default().fg(text_color))),
        ListItem::new(Span::styled("Table", if app.selected_entity.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
        ListItem::new(Span::styled("Filters", if app.selected_table.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
    ];
    if app.filters.len() > 1 {
        left_items.push(ListItem::new(Span::styled("Filters Op", Style::default().fg(text_color))));
    }
    left_items.extend(vec![
        ListItem::new(Span::styled("Aggregations", if app.selected_table.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
        ListItem::new(Span::styled("Order By (Strict)", if app.selected_table.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
        ListItem::new(Span::styled("Limit", if app.selected_table.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
        ListItem::new(Span::styled("Fields", if app.selected_table.is_some() { Style::default().fg(text_color) } else { Style::default().fg(inactive_color) })),
    ]);

    let left_list = List::new(left_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Left { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(left_list, panel_layout[0], &mut app.left_panel_state);

    let middle_items: Vec<ListItem> = match app.search_criteria {
        SearchCriteria::Entity => app.search_entities.iter().map(|s| {
            let circle = if Some(s.clone()) == app.selected_entity { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
        }).collect(),
        SearchCriteria::Table => app.search_tables.iter().map(|s| {
            let circle = if Some(s.clone()) == app.selected_table { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
        }).collect(),
        SearchCriteria::Filters => {
            let mut items: Vec<ListItem> = app.filters.iter().map(|f| {
                ListItem::new(Span::styled(format!("{} {} {}", f.field, f.op.as_str(), f.value), Style::default().fg(active_color)))
            }).collect();
            let add_color = if dimmed { Color::Indexed(235) } else { colors::ADD_COLOR };
            let del_color = if dimmed { Color::Indexed(235) } else { colors::DELETE_COLOR };
            items.push(ListItem::new(Span::styled("+ Add Next Filter", Style::default().fg(add_color))));
            if !app.filters.is_empty() { items.push(ListItem::new(Span::styled("- Delete Filter", Style::default().fg(del_color)))); }
            items
        },
        SearchCriteria::Aggregations => {
            let mut items: Vec<ListItem> = app.aggregations.iter().enumerate().map(|(i, agg)| {
                let agg_type = agg.as_object().and_then(|o| o.keys().next()).map(|s| s.as_str()).unwrap_or("Unknown");
                ListItem::new(format!("Agg {}: {}", i + 1, agg_type))
            }).collect();
            let add_color = if dimmed { Color::Indexed(235) } else { colors::ADD_COLOR };
            let del_color = if dimmed { Color::Indexed(235) } else { colors::DELETE_COLOR };
            items.push(ListItem::new(Span::styled("+ Add Next Aggregation", Style::default().fg(add_color))));
            if !app.aggregations.is_empty() { items.push(ListItem::new(Span::styled("- Delete Aggregation", Style::default().fg(del_color)))); }
            items
        },
        SearchCriteria::OrderBy => {
            let mut items: Vec<ListItem> = app.order_by.iter().enumerate().map(|(i, o)| {
                ListItem::new(format!("{}: {} {}", i + 1, o.field, o.direction.as_str()))
            }).collect();
            let add_color = if dimmed { Color::Indexed(235) } else { colors::ADD_COLOR };
            let del_color = if dimmed { Color::Indexed(235) } else { colors::DELETE_COLOR };
            items.push(ListItem::new(Span::styled("+ Add Next OrderBy", Style::default().fg(add_color))));
            if !app.order_by.is_empty() { items.push(ListItem::new(Span::styled("- Delete OrderBy", Style::default().fg(del_color)))); }
            items
        },
        SearchCriteria::Limit => vec![ListItem::new(format!("Value: {}", app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string())))],
        SearchCriteria::Fields => get_base_fields(&app.available_fields).iter().map(|f| {
            let circle = if app.selected_fields.contains(f) { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", f))]))
        }).collect(),
        SearchCriteria::FiltersOp => vec![
            ListItem::new(Line::from(vec![if app.filters_op == crate::repl::state::LogicalOp::And { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(" And")])),
            ListItem::new(Line::from(vec![if app.filters_op == crate::repl::state::LogicalOp::Or { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(" Or")])),
        ],
    };

    let middle_list = List::new(middle_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Middle { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(middle_list, panel_layout[1], &mut app.middle_panel_state);

    if app.search_criteria == SearchCriteria::Filters && !app.filters.is_empty() {
        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
        if current_idx < app.filters.len() {
            let right_items = vec![ListItem::new("Field"), ListItem::new("Op"), ListItem::new("Value")];
            let right_list = List::new(right_items)
                .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color })))
                .highlight_style(Style::default().fg(active_color))
                .highlight_symbol("> ");
            f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);

            let extra_items: Vec<ListItem> = match app.filter_step {
                FilterStep::Field => get_filtered_fields(&app.available_fields).into_iter().map(|s| {
                    let circle = if app.filters[current_idx].field == s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                FilterStep::Op => ["Eq", "Ne", "In", "Like", "Gt", "Gte", "Lt", "Lte"].iter().map(|s| {
                    let circle = if app.filters[current_idx].op.as_str() == *s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                FilterStep::Value => app.filter_value_options.iter().map(|s| {
                    let circle = if app.filters[current_idx].value == *s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                _ => vec![],
            };
            let extra_list = List::new(extra_items)
                .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color })))
                .highlight_style(Style::default().fg(active_color))
                .highlight_symbol("> ");
            f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);
        }
    } else if app.search_criteria == SearchCriteria::Aggregations && !app.aggregations.is_empty() {
        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
        if current_idx < app.aggregations.len() {
            let mut right_items = vec![ListItem::new("Change Type")];
            if let Some(inner) = app.aggregations[current_idx].as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                for key in inner.keys() { right_items.push(ListItem::new(key.as_str())); }
            }
            let right_list = List::new(right_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);

            let mut extra_items: Vec<ListItem> = Vec::new();
            let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);
            if selected_step_idx == 0 {
                extra_items = app.agg_type_options.iter().map(|s| {
                    let is_selected = app.aggregations[current_idx].as_object().and_then(|o| o.keys().next()) == Some(s);
                    ListItem::new(Line::from(vec![if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(format!(" {}", s))]))
                }).collect();
            } else if let Some(inner) = app.aggregations[current_idx].as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                let keys: Vec<&String> = inner.keys().collect();
                if let Some(key) = keys.get(selected_step_idx - 1) {
                    match key.as_str() {
                        "field" | "key_field" | "bucket_field" | "value_field" => {
                            extra_items = get_filtered_fields(&app.available_fields).into_iter().map(|s| {
                                let is_selected = inner.get(*key).and_then(|v| v.as_str()) == Some(&s);
                                ListItem::new(Line::from(vec![if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(format!(" {}", s))]))
                            }).collect();
                        }
                        "operation" => {
                            extra_items = app.agg_op_options.iter().map(|s| {
                                let is_selected = inner.get("operation").and_then(|v| v.as_str()) == Some(s);
                                ListItem::new(Line::from(vec![if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(format!(" {}", s))]))
                            }).collect();
                        }
                        _ => {
                            extra_items = app.agg_value_options.iter().map(|s| {
                                let is_selected = inner.get(*key).map(|v| v.to_string().replace("\"", "")) == Some(s.clone());
                                ListItem::new(Line::from(vec![if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(format!(" {}", s))]))
                            }).collect();
                        }
                    }
                }
            }
            let extra_list = List::new(extra_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);
        }
    } else if app.search_criteria == SearchCriteria::OrderBy && !app.order_by.is_empty() {
        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
        if current_idx < app.order_by.len() {
            let right_items = vec![ListItem::new("Field"), ListItem::new("Direction")];
            let right_list = List::new(right_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);
            let extra_items: Vec<ListItem> = match app.right_panel_state.selected() {
                 Some(0) => get_order_by_fields(&app.available_fields).into_iter().map(|f| {
                     ListItem::new(f.clone())
                 }).collect(),
                 Some(1) => vec![ListItem::new("Asc"), ListItem::new("Desc")],
                 _ => vec![],
            };
            let extra_list = List::new(extra_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { if dimmed { Color::Indexed(242) } else { colors::SELECTED_BORDER_COLOR } } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);
        }
    }

    let desc_style = Style::default().fg(instruction_color);
    let help_text = match app.focus_panel {
        FocusPanel::Left => "↑↓ Navigate  •  → Next Panel  •  esq Quit".to_string(),
        FocusPanel::Middle => "↑↓ Navigate  •  ↵ Toggle Selection  •  ←→ Switch Panel  •  esq Quit".to_string(),
        FocusPanel::Right => "↑↓ Navigate  •  ↵ Toggle Selection  •  ←→ Switch Panel  •  esq Quit".to_string(),
        FocusPanel::Extra => "↑↓ Navigate  •  ↵ Toggle Selection  •  ← Prev Panel  •  esq Quit".to_string(),
        FocusPanel::Bottom => "↵ Accept  •  esq Cancel".to_string(),
    };
    f.render_widget(Paragraph::new(help_text).style(desc_style).alignment(Alignment::Right), chunks[2]);

    if app.focus_panel == FocusPanel::Bottom {
        draw_input_widget(f, app, chunks[3], "Type filter value...", active_color, inactive_color);
    }

    // New instruction aligned with Query
    f.render_widget(Paragraph::new(vec![
        Line::from(vec![
            Span::styled("+ ", Style::default().fg(active_color).add_modifier(Modifier::BOLD)),
            Span::styled("To perform a query press the s key", Style::default().fg(text_color)),
        ]),
        Line::from(vec![
            Span::styled("+ ", Style::default().fg(active_color).add_modifier(Modifier::BOLD)),
            Span::styled("Press 'L' (Shift+l) to load a saved query", Style::default().fg(text_color)),
        ]),
        Line::from(vec![
            Span::styled("+ ", Style::default().fg(active_color).add_modifier(Modifier::BOLD)),
            Span::styled("Press 'E' (Shift+e) to edit a saved query", Style::default().fg(text_color)),
        ])
    ]), chunks[5]);

    draw_query_preview_styled(f, app, chunks[7], key_color, value_color, active_color);
}

fn draw_main_menu(f: &mut Frame, app: &mut App, area: Rect, dimmed: bool) {
    let purple = if dimmed { Color::Indexed(237) } else { colors::PRIMARY_COLOR };
    let purple_muted = if dimmed { Color::Indexed(235) } else { colors::MUTED_COLOR };
    let text_color = if dimmed { Color::Indexed(237) } else { Color::White };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(0)])
        .split(area);
    draw_menu_widget(f, app, chunks[0], purple, purple_muted, text_color);
    let loaded_data = get_loaded_data();
    if !loaded_data.is_empty() { draw_loaded_data_widget(f, &loaded_data, chunks[1]); }
}

fn draw_menu_widget(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color, text_color: Color) {
    let menu_block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(purple_muted)).padding(Padding::new(2, 2, 1, 1));
    let items: Vec<ListItem> = app.menu_items.iter().enumerate().map(|(i, m)| {
        ListItem::new(Span::styled(format!("{}. {}", i + 1, m), Style::default().fg(text_color)))
    }).collect();
    let list = List::new(items).block(menu_block).highlight_style(Style::default().fg(purple)).highlight_symbol("◉ ");
    f.render_stateful_widget(list, area, &mut app.menu_state);
}

fn draw_loaded_data_widget(f: &mut Frame, data: &[String], area: Rect) {
    let items: Vec<ListItem> = data.iter().map(|s| ListItem::new(Span::styled(s, Style::default().fg(Color::DarkGray)))).collect();
    let list = List::new(items).block(Block::default().padding(Padding::new(0, 0, 1, 0)));
    f.render_widget(list, area);
}

fn draw_input_widget(f: &mut Frame, app: &mut App, area: Rect, placeholder: &str, _color: Color, purple_muted: Color) {
    let is_focused = if app.active_task == Some("Search") { app.focus_panel == FocusPanel::Bottom } else { true };
    let input_block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(if is_focused { colors::SAND } else { purple_muted }));
    f.render_widget(&input_block, area);
    let inner = input_block.inner(area).inner(&Margin { vertical: 0, horizontal: 1 });
    let centered_area = Rect { x: inner.x, y: inner.y + (inner.height / 2), width: inner.width, height: 1 };
    let prompt_str = " > ";
    let mut spans = vec![Span::styled(prompt_str, Style::default().fg(colors::SAND).add_modifier(Modifier::BOLD))];
    let buffer = if app.active_task == Some("Search") { &app.filter_value_input } else { &app.input_buffer };
    if buffer.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(placeholder, Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(buffer));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), centered_area);
    if !buffer.is_empty() { f.set_cursor(centered_area.x + prompt_str.chars().count() as u16 + 1 + buffer.chars().count() as u16, centered_area.y); }
}

fn draw_suggestions_widget(f: &mut Frame, app: &mut App, area: Rect, purple: Color) {
    let items: Vec<ListItem> = app.suggestions.iter().map(|m| ListItem::new(Span::styled(m.as_str(), Style::default().fg(Color::DarkGray)))).collect();
    let mut state = ListState::default();
    state.select(app.suggestion_index);
    let list = List::new(items).highlight_style(Style::default().fg(purple).add_modifier(Modifier::BOLD)).highlight_symbol("   > ");
    f.render_stateful_widget(list, area, &mut state);
}

fn get_menu_lines(app: &App, purple: Color, purple_muted: Color, text_color: Color) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let width = 40; // Ancho total del contenedor
    
    // Borde superior
    lines.push(Line::from(Span::styled(format!("┌{}┐", "─".repeat(width - 2)), Style::default().fg(purple_muted))));
    
    for (i, item) in app.menu_items.iter().enumerate() {
        let is_selected = app.menu_state.selected() == Some(i);
        let bullet = if is_selected { "◉ " } else { "  " };
        let text = format!("{}. {}", i + 1, item);
        
        // Cálculo de padding:
        // │ (1) + espacio (1) + bullet (2 visual) + texto (N) + padding (P) + espacio (1) + │ (1) = width
        // 1 + 1 + 2 + text.len() + P + 1 + 1 = width
        // 6 + text.len() + P = width  =>  P = width - 6 - text.len()
        let padding_len = width.saturating_sub(6 + text.len());
        let padding = " ".repeat(padding_len);
        
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(purple_muted)),
            Span::styled(bullet, if is_selected { Style::default().fg(purple) } else { Style::default().fg(text_color) }),
            Span::styled(text, if is_selected { Style::default().fg(purple) } else { Style::default().fg(text_color) }),
            Span::styled(padding, Style::default()),
            Span::styled(" │", Style::default().fg(purple_muted)),
        ]));
    }
    
    // Borde inferior
    lines.push(Line::from(Span::styled(format!("└{}┘", "─".repeat(width - 2)), Style::default().fg(purple_muted))));
    lines
}

fn draw_query_preview_styled(f: &mut Frame, app: &mut App, area: Rect, key_style: Color, val_style: Color, active_style: Color) {
    let lines = get_query_preview_lines_styled(app, key_style, val_style, active_style);
    f.render_widget(Paragraph::new(lines), area);
}

pub fn get_query_preview_lines(app: &App) -> Vec<Line<'_>> {
    get_query_preview_lines_styled(app, colors::KEY_COLOR, colors::VALUE_COLOR, colors::ACTIVE_COLOR)
}

pub fn get_query_preview_lines_styled(app: &App, key_c: Color, val_c: Color, active_c: Color) -> Vec<Line<'_>> {
    let key_style = Style::default().fg(key_c);
    let val_style = Style::default().fg(val_c);
    let branch_style = Style::default().fg(key_c);
    let mut lines = Vec::new();

    let title = if app.search_results.is_some() { "Query" } else { "Query Preview" };
    lines.push(Line::from(Span::styled(title, Style::default().fg(active_c))));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Entity: ", key_style), Span::styled(app.selected_entity.as_deref().unwrap_or("?"), val_style)]));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Table: ", key_style), Span::styled(app.selected_table.as_deref().unwrap_or("?"), val_style)]));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Filters: ", key_style)]));
    if !app.filters.is_empty() {
        for (i, f) in app.filters.iter().enumerate() {
             lines.push(Line::from(vec![Span::styled(if i == app.filters.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(format!("{} {} {}", f.field, f.op.as_str(), f.value), val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(if key_c == colors::KEY_COLOR { Color::DarkGray } else { key_c }))])); }
    if app.filters.len() > 1 { lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Filters Op: ", key_style), Span::styled(app.filters_op.to_string(), val_style)])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Aggregations: ", key_style)]));
    if !app.aggregations.is_empty() {
        for (i, agg) in app.aggregations.iter().enumerate() {
            let agg_str = serde_json::to_string(agg).unwrap_or_default();
            lines.push(Line::from(vec![Span::styled(if i == app.aggregations.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(agg_str, val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(if key_c == colors::KEY_COLOR { Color::DarkGray } else { key_c }))])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Order By: ", key_style)]));
    if !app.order_by.is_empty() {
        for (i, o) in app.order_by.iter().enumerate() {
             lines.push(Line::from(vec![Span::styled(if i == app.order_by.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(format!("{} {}", o.field, o.direction.as_str()), val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(if key_c == colors::KEY_COLOR { Color::DarkGray } else { key_c }))])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Limit: ", key_style), Span::styled(app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string()), val_style)]));
    lines.push(Line::from(vec![Span::styled("└── ", branch_style), Span::styled("Fields: ", key_style), Span::styled(if app.selected_fields.is_empty() { "All".to_string() } else { format!("{:?}", app.selected_fields) }, val_style)]));
    lines
}

fn draw_save_query_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().title(" Save Query ").borders(Borders::ALL).border_type(BorderType::Rounded).style(Style::default().bg(Color::Rgb(0, 0, 0)));
    let area = centered_rect(60, 20, area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);
    
    let input = Paragraph::new(app.save_query_name_input.as_str())
        .block(Block::default().borders(Borders::ALL).title(" Query Name "));
    f.render_widget(input, chunks[0]);
    
    let msg = Paragraph::new("Press Enter to Save, Esc to Cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(msg, chunks[1]);
}

fn draw_saved_queries_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default().title(" Saved Queries ").borders(Borders::ALL).border_type(BorderType::Rounded).style(Style::default().bg(Color::Rgb(0, 0, 0)));
    let area = centered_rect(60, 60, area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = app.saved_queries.iter().map(|q| {
        ListItem::new(format!("{} ({} - {})", q.name, q.entity, q.table))
    }).collect();
    
    let list = List::new(items)
        .highlight_style(Style::default().fg(colors::ACTIVE_COLOR).add_modifier(Modifier::BOLD))
        .highlight_symbol("> ");
    f.render_stateful_widget(list, chunks[0], &mut app.saved_queries_state);

    let msg = Paragraph::new("Enter: Load & Run • d: Delete • Esc: Close")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(msg, chunks[1]);
}

fn draw_variable_prompt_overlay(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .title(" Variable Required ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(Color::Rgb(0, 0, 0)));
    
    let popup_area = centered_rect(50, 20, area);
    f.render_widget(Clear, popup_area);
    f.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(2)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(popup_area);
    
    let input = Paragraph::new(app.variable_input.as_str())
        .block(Block::default()
            .borders(Borders::ALL)
            .title(format!(" Enter value for {} ", app.current_variable)));
    f.render_widget(input, chunks[0]);
    
    let msg = Paragraph::new("Press Enter to continue, Esc to cancel")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(msg, chunks[1]);
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