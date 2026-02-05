use crate::repl::state::{App, FocusPanel, SearchCriteria, FilterStep, LoadStep};
use crate::repl::utils::get_loaded_data;
use ratatui::layout::Margin;
use ratatui::{prelude::*, widgets::*};

pub fn ui(f: &mut Frame, app: &mut App, purple: Color) {
    let purple_muted = Color::Rgb(244, 230, 255);
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
    let purple = Color::Rgb(197, 137, 249);
    let purple_muted = Color::Rgb(244, 230, 255);
    let active_color = Color::Rgb(137, 180, 249);
    let inactive_color = Color::Rgb(244, 230, 255);

    let content_height = 10; // Altura fija reducida para mayor simetría y compacidad

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // Menú
            Constraint::Length(content_height), // Paneles
            Constraint::Length(1), // Instrucciones justo debajo
            Constraint::Length(15), // Preview de Query (Aumentado de 10 a 15)
            Constraint::Length(1), // Espacio de separación
            Constraint::Length(if app.focus_panel == FocusPanel::Bottom { 3 } else { 0 }), // Input inferior
            Constraint::Min(0),    // Resto del espacio al final
        ])
        .split(area);
    
    // Aquí mantenemos los colores PÚRPURA para el menú superior
    draw_menu_widget(f, app, chunks[0], purple, purple_muted);
    
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
        Layout::default().direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(20), 
                Constraint::Percentage(20), 
                Constraint::Percentage(20), 
                Constraint::Percentage(40)
            ])
            .split(chunks[1])
    } else if app.search_criteria == SearchCriteria::OrderBy {
        Layout::default().direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(25), 
                Constraint::Percentage(25), 
                Constraint::Percentage(50)
            ])
            .split(chunks[1])
    } else {
        Layout::default().direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1])
    };

    // Panel Izquierdo dinámico
    let mut left_items = vec![
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

    if app.filters.len() > 1 {
        left_items.push(ListItem::new(Span::styled(
            "Filters Op",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )));
    }

    left_items.extend(vec![
        ListItem::new(Span::styled(
            "Aggregations",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
        ListItem::new(Span::styled(
            "Order By",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
        ListItem::new(Span::styled(
            "Limit",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
        ListItem::new(Span::styled(
            "Fields",
            if app.selected_table.is_some() { Style::default() } else { Style::default().fg(Color::DarkGray) }
        )),
    ]);

    let left_list = List::new(left_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Left { active_color } else { inactive_color })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(left_list, panel_layout[0], &mut app.left_panel_state);

    // Panel Medio (Panel 1)
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
        SearchCriteria::Filters => {
            let mut items: Vec<ListItem> = app.filters.iter().map(|f| {
                ListItem::new(Span::styled(
                    format!("{} {} {}", f.field, f.op, f.value),
                    Style::default().fg(active_color)
                ))
            }).collect();
            items.push(ListItem::new(Span::styled("+ Add Next Filter", Style::default().fg(Color::Rgb(130, 200, 160)))));
            if !app.filters.is_empty() {
                items.push(ListItem::new(Span::styled("- Delete Filter", Style::default().fg(Color::Rgb(249, 137, 197)))));
            }
            items
        },
        SearchCriteria::Aggregations => {
            let mut items: Vec<ListItem> = app.aggregations.iter().enumerate().map(|(i, agg)| {
                let agg_type = agg.as_object()
                    .and_then(|o| o.keys().next())
                    .map(|s| s.as_str())
                    .unwrap_or("Unknown");
                ListItem::new(format!("Agg {}: {}", i + 1, agg_type))
            }).collect();
            items.push(ListItem::new(Span::styled("+ Add Next Aggregation", Style::default().fg(Color::Rgb(130, 200, 160)))));
            if !app.aggregations.is_empty() {
                items.push(ListItem::new(Span::styled("- Delete Aggregation", Style::default().fg(Color::Rgb(249, 137, 197)))));
            }
            items
        },
        SearchCriteria::OrderBy => vec![
            ListItem::new(format!("Field: {}", app.order_by.as_ref().map(|o| o.field.as_str()).unwrap_or("?"))),
            ListItem::new(format!("Direction: {}", app.order_by.as_ref().map(|o| o.direction.as_str()).unwrap_or("?"))),
        ],
        SearchCriteria::Limit => vec![
            ListItem::new(format!("Value: {}", app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string()))),
        ],
        SearchCriteria::Fields => {
            let mut items: Vec<ListItem> = app.available_fields.iter().map(|f| {
                let is_selected = app.selected_fields.contains(f);
                let circle = if is_selected {
                    Span::styled("◉", Style::default().fg(active_color))
                } else {
                    Span::raw("○")
                };
                ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", f))]))
            }).collect();
            if items.is_empty() {
                items.push(ListItem::new(Span::styled("No fields available", Style::default().fg(Color::DarkGray))));
            }
            items
        },
        SearchCriteria::FiltersOp => vec![
            ListItem::new(Line::from(vec![
                if app.filters_op == "And" { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },
                Span::raw(" And")
            ])),
            ListItem::new(Line::from(vec![
                if app.filters_op == "Or" { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },
                Span::raw(" Or")
            ])),
        ],
    };
    let middle_list = List::new(middle_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::new(1, 1, 1, 1))
            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Middle { active_color } else { inactive_color })))
        .highlight_style(Style::default().fg(active_color))
        .highlight_symbol("> ");
    f.render_stateful_widget(middle_list, panel_layout[1], &mut app.middle_panel_state);

            // Panel Derecho (Panel 2) y Extra (Panel 3)

            if app.search_criteria == SearchCriteria::Filters && !app.filters.is_empty() {

                let current_idx = app.middle_panel_state.selected().unwrap_or(0);

                if current_idx < app.filters.len() {

                    // ... (resto del código de renderizado de filtros)

                    // Panel 2: Steps

                    let right_items = vec![

                        ListItem::new("Field"),

                        ListItem::new("Op"),

                        ListItem::new("Value"),

                    ];

                    let right_list = List::new(right_items)

                        .block(Block::default()

                            .borders(Borders::ALL)

                            .border_type(BorderType::Rounded)

                            .padding(Padding::new(1, 1, 1, 1))

                            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { active_color } else { inactive_color })))

                        .highlight_style(Style::default().fg(active_color))

                        .highlight_symbol("> ");

                    f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);

        

                    // Panel 3: Step Options

                    let extra_items: Vec<ListItem> = match app.filter_step {

                        FilterStep::Field => app.available_fields.iter().map(|s| {

                            let is_selected = app.filters[current_idx].field == *s;

                            let circle = if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };

                            ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))

                        }).collect(),

                        FilterStep::Op => {

                            let ops = vec!["Eq", "In", "Gte", "Lt"];

                            ops.iter().map(|s| {

                                let is_selected = app.filters[current_idx].op == *s;

                                let circle = if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };

                                ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))

                            }).collect()

                        },

                                                FilterStep::Value => {

                                                    app.filter_value_options.iter().map(|s| {

                                                        let is_selected = app.filters[current_idx].value == *s;

                                                        let circle = if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") };

                                                        ListItem::new(Line::from(vec![circle, Span::raw(format!(" {}", s))]))

                                                    }).collect()

                                                },

                                                _ => vec![],

                                            };

                    let extra_list = List::new(extra_items)

                        .block(Block::default()

                            .borders(Borders::ALL)

                            .border_type(BorderType::Rounded)

                            .padding(Padding::new(1, 1, 1, 1))

                            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { active_color } else { inactive_color })))

                        .highlight_style(Style::default().fg(active_color))

                        .highlight_symbol("> ");

                    f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);

                }

            } else if app.search_criteria == SearchCriteria::Aggregations && !app.aggregations.is_empty() {

                let current_idx = app.middle_panel_state.selected().unwrap_or(0);

                if current_idx < app.aggregations.len() {

                    // Panel 2 (C3): Steps dinámicos basados en las llaves del JSON

                    let mut right_items = vec![ListItem::new("Change Type")];

                    if let Some(agg) = app.aggregations.get(current_idx) {

                        if let Some(inner) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {

                            for key in inner.keys() {

                                right_items.push(ListItem::new(key.as_str()));

                            }

                        }

                    }

        

                    let right_list = List::new(right_items)

                        .block(Block::default()

                            .borders(Borders::ALL)

                            .border_type(BorderType::Rounded)

                            .padding(Padding::new(1, 1, 1, 1))

                            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Right { active_color } else { inactive_color })))

                        .highlight_style(Style::default().fg(active_color))

                        .highlight_symbol("> ");

                    f.render_stateful_widget(right_list, panel_layout[2], &mut app.right_panel_state);

        

                    // Panel 3 (C4): Opciones para la llave seleccionada

                    let mut extra_items: Vec<ListItem> = Vec::new();

                    let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);

                    

                    if selected_step_idx == 0 {

                         // Change Type

                         extra_items = app.agg_type_options.iter().map(|s| {

                            let is_selected = app.aggregations[current_idx].as_object().and_then(|o| o.keys().next()) == Some(s);

                            ListItem::new(Line::from(vec![

                                if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },

                                Span::raw(format!(" {}", s))

                            ]))

                         }).collect();

                    } else if let Some(agg) = app.aggregations.get(current_idx) {

                        if let Some(inner) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {

                            let keys: Vec<&String> = inner.keys().collect();

                            if let Some(key) = keys.get(selected_step_idx - 1) {

                                match key.as_str() {

                                    "field" | "key_field" | "bucket_field" | "value_field" => {

                                        extra_items = app.available_fields.iter().map(|s| {

                                            let is_selected = inner.get(*key).and_then(|v| v.as_str()) == Some(s);

                                            ListItem::new(Line::from(vec![

                                                if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },

                                                Span::raw(format!(" {}", s))

                                            ]))

                                        }).collect();

                                    }

                                    "operation" => {

                                         extra_items = app.agg_op_options.iter().map(|s| {

                                            let is_selected = inner.get("operation").and_then(|v| v.as_str()) == Some(s);

                                            ListItem::new(Line::from(vec![

                                                if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },

                                                Span::raw(format!(" {}", s))

                                            ]))

                                        }).collect();

                                    }

                                    _ => {

                                        // Mostrar agg_value_options (Write value + history)

                                        extra_items = app.agg_value_options.iter().map(|s| {

                                            let is_selected = inner.get(*key).map(|v| v.to_string().replace("\"", "")) == Some(s.clone());

                                            ListItem::new(Line::from(vec![

                                                if is_selected { Span::styled("◉", Style::default().fg(active_color)) } else { Span::raw("○") },

                                                Span::raw(format!(" {}", s))

                                            ]))

                                        }).collect();

                                    }

                                }

                            }

                        }

                    }

        

                    let extra_list = List::new(extra_items)

                        .block(Block::default()

                            .borders(Borders::ALL)

                            .border_type(BorderType::Rounded)

                            .padding(Padding::new(1, 1, 1, 1))

                            .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { active_color } else { inactive_color })))

                        .highlight_style(Style::default().fg(active_color))

                        .highlight_symbol("> ");

                    f.render_stateful_widget(extra_list, panel_layout[3], &mut app.extra_panel_state);

                }

            } else if app.search_criteria == SearchCriteria::OrderBy {
        let extra_items = match app.middle_panel_state.selected() {
            Some(0) => app.available_fields.iter().map(|f| ListItem::new(f.as_str())).collect(),
            Some(1) => vec![ListItem::new("Asc"), ListItem::new("Desc")],
            _ => vec![],
        };
        let extra_list = List::new(extra_items)
            .block(Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .padding(Padding::new(1, 1, 1, 1))
                .border_style(Style::default().fg(if app.focus_panel == FocusPanel::Extra { active_color } else { inactive_color })))
            .highlight_style(Style::default().fg(active_color))
            .highlight_symbol("> ");
        f.render_stateful_widget(extra_list, panel_layout[2], &mut app.extra_panel_state);
    }

    // Instrucciones minimalistas con descripciones claras
    let desc_style = Style::default().fg(purple);
    let separator = "  •  ";

    let help_text = match app.focus_panel {
        FocusPanel::Left => format!("↑↓ Navigate{}→ Next Panel{}esq Quit", separator, separator),
        FocusPanel::Middle => format!("↑↓ Navigate{}↵ Toggle Selection{}←→ Switch Panel{}esq Quit", separator, separator, separator),
        FocusPanel::Right => format!("↑↓ Navigate{}↵ Toggle Selection{}←→ Switch Panel{}esq Quit", separator, separator, separator),
        FocusPanel::Extra => format!("↑↓ Navigate{}↵ Toggle Selection{}← Prev Panel{}esq Quit", separator, separator, separator),
        FocusPanel::Bottom => format!("↵ Accept{}esq Cancel", separator),
    };
    
    let instructions_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(1)]) 
        .split(chunks[2]);

    f.render_widget(
        Paragraph::new(help_text).style(desc_style).alignment(Alignment::Right),
        instructions_layout[0]
    );

    // Query Preview
    draw_query_preview(f, app, chunks[3]);

    // Input Inferior
    if app.focus_panel == FocusPanel::Bottom {
        draw_input_widget(f, app, chunks[5], "Type filter value...", active_color, inactive_color);
    }
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

    let buffer = if app.active_task == Some("Search") {
        &app.filter_value_input
    } else {
        &app.input_buffer
    };

    if buffer.is_empty() {
        spans.push(Span::styled(" ", Style::default().bg(Color::White)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(placeholder, Style::default().fg(Color::DarkGray)));
    } else {
        spans.push(Span::raw(" "));
        spans.push(Span::raw(buffer));
    }

    f.render_widget(Paragraph::new(Line::from(spans)), centered_area);
    if !buffer.is_empty() {
        f.set_cursor(centered_area.x + prompt_str.chars().count() as u16 + 1 + buffer.chars().count() as u16, centered_area.y);
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

fn draw_query_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let active_color = Color::Rgb(137, 180, 249);
    
    // Función auxiliar para colorear las claves y valores
    let key_style = Style::default().fg(Color::DarkGray);
    let val_style = Style::default().fg(Color::Cyan);
    let branch_style = Style::default().fg(Color::DarkGray);

    let mut lines = Vec::new();

    // Root
    lines.push(Line::from(Span::styled("Query Preview", Style::default().fg(active_color))));

    // Entity
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Entity: ", key_style),
        Span::styled(app.selected_entity.as_deref().unwrap_or("?"), val_style),
    ]));

    // Table
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Table: ", key_style),
        Span::styled(app.selected_table.as_deref().unwrap_or("?"), val_style),
    ]));

    // Filters
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Filters: ", key_style),
    ]));
    
    // Mostrar filtros guardados
    if !app.filters.is_empty() {
        for (i, f) in app.filters.iter().enumerate() {
             lines.push(Line::from(vec![
                Span::styled(if i == app.filters.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style),
                Span::styled(
                    format!("{} {} {}", f.field, f.op, f.value),
                    val_style
                ),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("│   └── ", branch_style),
            Span::styled("(None)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Filters Op
    if app.filters.len() > 1 {
        lines.push(Line::from(vec![
            Span::styled("├── ", branch_style),
            Span::styled("Filters Op: ", key_style),
            Span::styled(&app.filters_op, val_style),
        ]));
    }

    // Aggregations
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Aggregations: ", key_style),
    ]));
    if !app.aggregations.is_empty() {
        for (i, agg) in app.aggregations.iter().enumerate() {
            let agg_str = serde_json::to_string(agg).unwrap_or_default();
            lines.push(Line::from(vec![
                Span::styled(if i == app.aggregations.len() - 1 { "│   └── " } else { "│   ├── " }, branch_style),
                Span::styled(agg_str, val_style),
            ]));
        }
    } else {
        lines.push(Line::from(vec![
            Span::styled("│   └── ", branch_style),
            Span::styled("(None)", Style::default().fg(Color::DarkGray)),
        ]));
    }

    // Order By
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Order By: ", key_style),
        Span::styled(
            app.order_by.as_ref().map(|o| format!("{} {}", o.field, o.direction)).unwrap_or_else(|| "None".to_string()),
            val_style
        ),
    ]));

    // Limit
    lines.push(Line::from(vec![
        Span::styled("├── ", branch_style),
        Span::styled("Limit: ", key_style),
        Span::styled(app.limit.map(|l| l.to_string()).unwrap_or_else(|| "None".to_string()), val_style),
    ]));

    // Fields
    lines.push(Line::from(vec![
        Span::styled("└── ", branch_style),
        Span::styled("Fields: ", key_style),
        Span::styled(
            if app.selected_fields.is_empty() { "All".to_string() } else { format!("{:?}", app.selected_fields) },
            val_style
        ),
    ]));

    f.render_widget(Paragraph::new(lines), area);
}
