use crate::repl::state::{App, LoadStep};
use crate::repl::utils::get_loaded_data;
use ratatui::layout::Margin;
use ratatui::{prelude::*, widgets::*};

pub fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
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
    let action_height = 3;

    // 2. Renderizar Datos Cargados
    let loaded_data = get_loaded_data();

    // Determinamos el layout según si hay tarea activa o no
    if app.active_task.is_none() {
        // MODO MENÚ: Menú + Lista de Datos Cargados
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7), // Menú
                Constraint::Min(0),    // Datos cargados (todo el espacio restante)
            ])
            .split(central_area);

        // 1. Renderizar Menú
        let menu_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted))
            .padding(Padding::new(2, 2, 1, 1));

        let items: Vec<ListItem> = app
            .menu_items
            .iter()
            .enumerate()
            .map(|(i, m)| ListItem::new(format!("{}. {}", i + 1, m)))
            .collect();

        let list = List::new(items)
            .block(menu_block)
            .highlight_style(Style::default().fg(purple))
            .highlight_symbol("◉ ");

        f.render_stateful_widget(list, chunks[0], &mut app.menu_state);

        // 2. Renderizar Datos Cargados
        if !loaded_data.is_empty() {
            let loaded_items: Vec<ListItem> = loaded_data
                .iter()
                .map(|s| ListItem::new(Span::styled(s, Style::default().fg(Color::DarkGray))))
                .collect();

            let loaded_list =
                List::new(loaded_items).block(Block::default().padding(Padding::new(0, 0, 1, 0)));

            f.render_widget(loaded_list, chunks[1]);
        }
    } else {
        // MODO TAREA: Menú + Datos Cargados + Espacio + Input + Sugerencias

        let loaded_height = if loaded_data.is_empty() {
            0
        } else {
            (loaded_data.len() as u16).min(8) + 1
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(7), // Menú (alto fijo)
                Constraint::Length(loaded_height), // Datos cargados (altura dinámica limitada)
                Constraint::Length(1), // Espacio de separación (AJUSTADO)
                Constraint::Length(action_height), // Barra de input (3 líneas)
                Constraint::Min(0),    // Sugerencias inmediatamente debajo
            ])
            .split(central_area);

        // 1. Renderizar Menú

        let menu_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted))
            .padding(Padding::new(2, 2, 1, 1));

        let items: Vec<ListItem> = app
            .menu_items
            .iter()
            .enumerate()
            .map(|(i, m)| ListItem::new(format!("{}. {}", i + 1, m)))
            .collect();

        let list = List::new(items)
            .block(menu_block)
            .highlight_style(Style::default().fg(purple))
            .highlight_symbol("◉ ");

        f.render_stateful_widget(list, chunks[0], &mut app.menu_state);

        // 2. Renderizar Datos Cargados (Si existen)

        if !loaded_data.is_empty() {
            let loaded_items: Vec<ListItem> = loaded_data
                .iter()
                .map(|s| ListItem::new(Span::styled(s, Style::default().fg(Color::DarkGray))))
                .collect();

            let loaded_list =
                List::new(loaded_items).block(Block::default().padding(Padding::new(0, 0, 1, 0)));

            f.render_widget(loaded_list, chunks[1]);
        }

        // 3. RENDERIZAR BARRA DE ENTRADA Y ESTADOS

        match app.load_step {
            LoadStep::Processing => {
                let area = centered_rect(60, 20, f.size());

                f.render_widget(Clear, area);

                f.render_widget(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .title(" Cargando ")
                        .border_style(Style::default().fg(purple)),
                    area,
                );

                f.render_widget(
                    Paragraph::new("\nAnalizando y procesando archivos...\nPor favor espera.")
                        .alignment(Alignment::Center),
                    area.inner(&Margin {
                        vertical: 1,

                        horizontal: 2,
                    }),
                );

                return;
            }

            LoadStep::Done => {}

            _ => {}
        }

        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(purple_muted));

        f.render_widget(&input_block, chunks[3]); // Input ahora en chunks[3] (antes era 2)

        let inner_area = input_block.inner(chunks[3]);

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

        let mut spans = vec![Span::styled(
            prompt_str,
            Style::default().fg(purple).add_modifier(Modifier::BOLD),
        )];

        if app.input_buffer.is_empty() {
            spans.push(Span::styled(" ", Style::default().bg(Color::White)));

            spans.push(Span::raw(" "));

            spans.push(Span::styled(
                placeholder,
                Style::default().fg(Color::DarkGray),
            ));
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

        // 4. RENDERIZAR SUGERENCIAS

        if !app.suggestions.is_empty() {
            let suggestions_items: Vec<ListItem> = app
                .suggestions
                .iter()
                .map(|m| {
                    ListItem::new(Span::styled(
                        m.as_str(),
                        Style::default().fg(Color::DarkGray),
                    ))
                })
                .collect();

            let mut suggestion_state = ListState::default();

            suggestion_state.select(app.suggestion_index);

            let list_widget = List::new(suggestions_items)
                .highlight_style(Style::default().fg(purple).add_modifier(Modifier::BOLD))
                .highlight_symbol("   > ");

            f.render_stateful_widget(list_widget, chunks[4], &mut suggestion_state);
            // Sugerencias ahora en chunks[4]
        }
    }
}
