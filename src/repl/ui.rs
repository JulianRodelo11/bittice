use crate::repl::state::{App, FocusPanel, SearchCriteria, FilterStep, LoadStep};
use crate::repl::utils::{get_loaded_data, get_order_by_fields, get_filtered_fields};
use crate::ui::colors;
use ratatui::layout::Margin;
use ratatui::{prelude::*, widgets::*};

pub fn ui(f: &mut Frame, app: &mut App, purple: Color) {
    let purple_muted = colors::MUTED_COLOR;
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
    let purple = colors::PRIMARY_COLOR;
    let purple_muted = colors::MUTED_COLOR;
    let active_color = colors::ACTIVE_COLOR;
    let inactive_color = colors::INACTIVE_COLOR;

    let content_height = if app.filters.len() > 1 { 12 } else { 11 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Menú
            Constraint::Length(content_height), // Paneles
            Constraint::Length(1), // Instrucciones justo debajo
            Constraint::Length(if app.focus_panel == FocusPanel::Bottom { 3 } else { 0 }), // Input inferior
            Constraint::Length(1), // Espacio de separación
            Constraint::Length(15), // Preview de Query
            Constraint::Min(0),    // Resto del espacio al final
        ])
        .split(area);
    
    draw_menu_widget(f, app, chunks[0], purple, purple_muted);

    // --- QUERY RESULTS MODE ---
    if let Some(results) = app.search_results.clone() {
        let query_h = 10;
        let table_h = (results.rows.len() as u16 * 2 + 3).min(area.height.saturating_sub(7 + 1 + 1 + 1 + query_h + 1 + 2));

        let results_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7),       // Menú [0]
                Constraint::Length(1),       // Spacer 0 [1]
                Constraint::Length(1),       // Instruction [2]
                Constraint::Length(1),       // Spacer 1 [3]
                Constraint::Length(query_h), // Query Details [4]
                Constraint::Length(1),       // Spacer 2 [5]
                Constraint::Length(table_h), // Results Table [6]
                Constraint::Min(0),          // Remaining
            ])
            .split(area);

        draw_menu_widget(f, app, results_layout[0], purple, purple_muted);
        
        // New instruction at the top
        f.render_widget(
            Paragraph::new("+ Press Esc to exit search results").style(Style::default().fg(Color::White)),
            results_layout[2]
        );

        draw_query_preview(f, app, results_layout[4]);

        let grid_color = Color::Rgb(100, 100, 100);
        
        // ... (col widths calculation same as before)
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

        let mut lines = Vec::new();
        let num_cols = results.headers.len();

        if num_cols > 0 {
            let mut top = String::from("┌");
            for (i, &w) in col_widths.iter().enumerate() {
                top.push_str(&"─".repeat(w));
                if i < num_cols - 1 { top.push('┬'); }
            }
            top.push('┐');
            lines.push(Line::from(Span::styled(top, Style::default().fg(grid_color))));

            let mut header_line = Vec::new();
            header_line.push(Span::styled("│", Style::default().fg(grid_color)));
            for (i, h) in results.headers.iter().enumerate() {
                let w = col_widths[i];
                let truncated = if h.len() > w - 2 { &h[..w - 2] } else { h };
                let cell = format!(" {:<width$} ", truncated, width = w - 2);
                header_line.push(Span::styled(cell, Style::default().fg(colors::ACTIVE_COLOR)));
                header_line.push(Span::styled("│", Style::default().fg(grid_color)));
            }
            lines.push(Line::from(header_line));

            let mut middle = String::from("├");
            for (i, &w) in col_widths.iter().enumerate() {
                middle.push_str(&"─".repeat(w));
                if i < num_cols - 1 { middle.push('┼'); }
            }
            middle.push('┤');
            lines.push(Line::from(Span::styled(middle.clone(), Style::default().fg(grid_color))));

            for (r_idx, row) in results.rows.iter().enumerate() {
                let mut row_line = Vec::new();
                row_line.push(Span::styled("│", Style::default().fg(grid_color)));
                for (i, cell_val) in row.iter().enumerate() {
                    let w = col_widths[i];
                    let truncated = if cell_val.len() > w - 2 { &cell_val[..w - 2] } else { cell_val };
                    let cell = format!(" {:<width$} ", truncated, width = w - 2);
                    row_line.push(Span::styled(cell, Style::default().fg(colors::VALUE_COLOR)));
                    row_line.push(Span::styled("│", Style::default().fg(grid_color)));
                }
                lines.push(Line::from(row_line));

                if r_idx < results.rows.len() - 1 {
                    lines.push(Line::from(Span::styled(middle.clone(), Style::default().fg(grid_color))));
                } else {
                    let mut bottom = String::from("└");
                    for (i, &w) in col_widths.iter().enumerate() {
                        bottom.push_str(&"─".repeat(w));
                        if i < num_cols - 1 { bottom.push('┴'); }
                    }
                    bottom.push('┘');
                    lines.push(Line::from(Span::styled(bottom, Style::default().fg(grid_color))));
                }
            }
        }

        f.render_widget(Paragraph::new(lines), results_layout[6]);
        return;
    }

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
        ListItem::new("Entity"),
        ListItem::new(Span::styled("Table", if app.selected_entity.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
        ListItem::new(Span::styled("Filters", if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
    ];
    if app.filters.len() > 1 {
        left_items.push(ListItem::new(Span::styled("Filters Op", Style::default())));
    }
    left_items.extend(vec![
        ListItem::new(Span::styled("Aggregations", if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
        ListItem::new(Span::styled("Order By", if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
        ListItem::new(Span::styled("Limit", if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
        ListItem::new(Span::styled("Fields", if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) })),
    ]);

    let left_list = List::new(left_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Left { colors::SELECTED_BORDER_COLOR } else { inactive_color })))
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
                ListItem::new(Span::styled(format!("{} {} {}", f.field, f.op, f.value), Style::default().fg(active_color)))
            }).collect();
            items.push(ListItem::new(Span::styled("+ Add Next Filter", Style::default().fg(colors::ADD_COLOR))));
            if !app.filters.is_empty() { items.push(ListItem::new(Span::styled("- Delete Filter", Style::default().fg(colors::DELETE_COLOR)))); }
            items
        },
        SearchCriteria::Aggregations => {
            let mut items: Vec<ListItem> = app.aggregations.iter().enumerate().map(|(i, agg)| {
                let agg_type = agg.as_object().and_then(|o| o.keys().next()).map(|s| s.as_str()).unwrap_or("Unknown");
                ListItem::new(format!("Agg {}: {}", i + 1, agg_type))
            }).collect();
            items.push(ListItem::new(Span::styled("+ Add Next Aggregation", Style::default().fg(colors::ADD_COLOR))));
            if !app.aggregations.is_empty() { items.push(ListItem::new(Span::styled("- Delete Aggregation", Style::default().fg(colors::DELETE_COLOR)))); }
            items
        },
        SearchCriteria::OrderBy => {
            let mut items: Vec<ListItem> = app.order_by.iter().enumerate().map(|(i, o)| {
                ListItem::new(format!("{}: {} {}", i + 1, o.field, o.direction))
            }).collect();
            items.push(ListItem::new(Span::styled("+ Add Next OrderBy", Style::default().fg(colors::ADD_COLOR))));
            if !app.order_by.is_empty() { items.push(ListItem::new(Span::styled("- Delete OrderBy", Style::default().fg(colors::DELETE_COLOR)))); }
            items
        },
        SearchCriteria::Limit => vec![ListItem::new(format!("Value: {}", app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string())))],
        SearchCriteria::Fields => app.available_fields.iter().map(|f| {
            let circle = if app.selected_fields.contains(f) { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", f))]))
        }).collect(),
        SearchCriteria::FiltersOp => vec![
            ListItem::new(Line::from(vec![if app.filters_op == "And" { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(" And")])),
            ListItem::new(Line::from(vec![if app.filters_op == "Or" { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") }, Span::raw(" Or")])),
        ],
    };

    let middle_list = List::new(middle_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Middle { colors::SELECTED_BORDER_COLOR } else { inactive_color })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(middle_list, panel_layout[1], &mut app.middle_panel_state);

    if app.search_criteria == SearchCriteria::Filters && !app.filters.is_empty() {
        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
        if current_idx < app.filters.len() {
            let right_items = vec![ListItem::new("Field"), ListItem::new("Op"), ListItem::new("Value")];
            let right_list = List::new(right_items)
                .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { colors::SELECTED_BORDER_COLOR } else { inactive_color })))
                .highlight_style(Style::default().fg(active_color))
                .highlight_symbol("> ");
            f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);

            let extra_items: Vec<ListItem> = match app.filter_step {
                FilterStep::Field => get_filtered_fields(&app.available_fields).into_iter().map(|s| {
                    let circle = if app.filters[current_idx].field == s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                FilterStep::Op => vec!["Eq", "In", "Gte", "Lt"].iter().map(|s| {
                    let circle = if app.filters[current_idx].op == *s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                FilterStep::Value => app.filter_value_options.iter().map(|s| {
                    let circle = if app.filters[current_idx].value == *s { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };
                    ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))
                }).collect(),
                _ => vec![],
            };
            let extra_list = List::new(extra_items)
                .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { colors::SELECTED_BORDER_COLOR } else { inactive_color })))
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
            let right_list = List::new(right_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { colors::SELECTED_BORDER_COLOR } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
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
            let extra_list = List::new(extra_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { colors::SELECTED_BORDER_COLOR } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);
        }
    } else if app.search_criteria == SearchCriteria::OrderBy && !app.order_by.is_empty() {
        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
        if current_idx < app.order_by.len() {
            let right_items = vec![ListItem::new("Field"), ListItem::new("Direction")];
            let right_list = List::new(right_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { colors::SELECTED_BORDER_COLOR } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);
            let extra_items: Vec<ListItem> = match app.right_panel_state.selected() {
                 Some(0) => get_order_by_fields(&app.available_fields).into_iter().map(ListItem::new).collect(),
                 Some(1) => vec![ListItem::new("Asc"), ListItem::new("Desc")],
                 _ => vec![],
            };
            let extra_list = List::new(extra_items).block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).padding(Padding::new(1, 1, 1, 1)).border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { colors::SELECTED_BORDER_COLOR } else { inactive_color }))).highlight_style(Style::default().fg(active_color)).highlight_symbol("> ");
            f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);
        }
    }

    let desc_style = Style::default().fg(colors::INSTRUCTION_COLOR);
    let help_text = match app.focus_panel {
        FocusPanel::Left => format!("↑↓ Navigate  •  → Next Panel  •  esq Quit"),
        FocusPanel::Middle => format!("↑↓ Navigate  •  ↵ Toggle Selection  •  ←→ Switch Panel  •  esq Quit"),
        FocusPanel::Right => format!("↑↓ Navigate  •  ↵ Toggle Selection  •  ←→ Switch Panel  •  esq Quit"),
        FocusPanel::Extra => format!("↑↓ Navigate  •  ↵ Toggle Selection  •  ← Prev Panel  •  esq Quit"),
        FocusPanel::Bottom => format!("↵ Accept  •  esq Cancel"),
    };
    f.render_widget(Paragraph::new(help_text).style(desc_style).alignment(Alignment::Right), chunks[2]);

    if app.focus_panel == FocusPanel::Bottom {
        draw_input_widget(f, app, chunks[3], "Type filter value...", active_color, inactive_color);
    }
    draw_query_preview(f, app, chunks[5]);
}

fn draw_main_menu(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);
    draw_menu_widget(f, app, chunks[0], purple, purple_muted);
    let loaded_data = get_loaded_data();
    if !loaded_data.is_empty() { draw_loaded_data_widget(f, &loaded_data, chunks[1]); }
}

fn draw_menu_widget(f: &mut Frame, app: &mut App, area: Rect, purple: Color, purple_muted: Color) {
    let menu_block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).border_style(Style::default().fg(purple_muted)).padding(Padding::new(2, 2, 1, 1));
    let items: Vec<ListItem> = app.menu_items.iter().enumerate().map(|(i, m)| ListItem::new(format!("{}. {}", i + 1, m))).collect();
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

fn draw_query_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let active_color = colors::ACTIVE_COLOR;
    let key_style = Style::default().fg(colors::KEY_COLOR);
    let val_style = Style::default().fg(colors::VALUE_COLOR);
    let branch_style = Style::default().fg(colors::KEY_COLOR);
    let mut lines = Vec::new();

    // Root
    let title = if app.search_results.is_some() { "Query" } else { "Query Preview" };
    lines.push(Line::from(Span::styled(title, Style::default().fg(active_color))));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Entity: ", key_style), Span::styled(app.selected_entity.as_deref().unwrap_or("?"), val_style)]));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Table: ", key_style), Span::styled(app.selected_table.as_deref().unwrap_or("?"), val_style)]));
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Filters: ", key_style)]));
    if !app.filters.is_empty() {
        for (i, f) in app.filters.iter().enumerate() {
             lines.push(Line::from(vec![Span::styled(if i == app.filters.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(format!("{} {} {}", f.field, f.op, f.value), val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(Color::DarkGray))])); }
    if app.filters.len() > 1 { lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Filters Op: ", key_style), Span::styled(&app.filters_op, val_style)])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Aggregations: ", key_style)]));
    if !app.aggregations.is_empty() {
        for (i, agg) in app.aggregations.iter().enumerate() {
            let agg_str = serde_json::to_string(agg).unwrap_or_default();
            lines.push(Line::from(vec![Span::styled(if i == app.aggregations.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(agg_str, val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(Color::DarkGray))])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Order By: ", key_style)]));
    if !app.order_by.is_empty() {
        for (i, o) in app.order_by.iter().enumerate() {
            lines.push(Line::from(vec![Span::styled(if i == app.order_by.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style), Span::styled(format!("{} {}", o.field, o.direction), val_style)]));
        }
    } else { lines.push(Line::from(vec![Span::styled("│   └── ", branch_style), Span::styled("(None)", Style::default().fg(Color::DarkGray))])); }
    lines.push(Line::from(vec![Span::styled("├── ", branch_style), Span::styled("Limit: ", key_style), Span::styled(app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string()), val_style)]));
    lines.push(Line::from(vec![Span::styled("└── ", branch_style), Span::styled("Fields: ", key_style), Span::styled(if app.selected_fields.is_empty() { "All".to_string() } else { format!("{:?}", app.selected_fields) }, val_style)]));
    f.render_widget(Paragraph::new(lines), area);
}