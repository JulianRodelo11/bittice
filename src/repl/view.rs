use ratatui::{
    layout::{Constraint, Direction, Layout, Rect, Position},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, BorderType, List, ListItem, Table as TuiTable, Row, Cell},
    Frame,
};
use crossterm::event::{self, KeyCode, MouseEventKind, MouseButton, KeyModifiers};
use serde_json::Value;
use crate::repl::state::{App, BitticePanel};
use crate::repl::utils::{get_catalog_tree, flatten_catalog_nodes, toggle_catalog_node, get_base_fields, get_indexed_fields};
use crate::core::storage::table::Table;
use std::path::Path;

const PRIMARY: Color = Color::Rgb(137, 180, 250);
const SECONDARY: Color = Color::Rgb(49, 50, 68);
const TEXT: Color = Color::Rgb(205, 214, 244);
const DARK: Color = Color::Rgb(30, 30, 46);
const SELECTION: Color = Color::Rgb(69, 71, 90);
const CURSOR_LINE: Color = Color::Rgb(40, 40, 55);

#[derive(serde::Deserialize)]
struct QueryPayload {
    entity: String,
    table: String,
    #[serde(default)]
    filters: Vec<crate::core::types::Filter>,
    #[serde(default)]
    fields: Vec<String>,
    #[serde(default)]
    order_by: Vec<crate::core::types::OrderBy>,
}

fn execute_query(app: &mut App) {
    let full_content = app.b_editor_lines.join("\n");
    let payload: QueryPayload = match serde_json::from_str(&full_content) {
        Ok(p) => p,
        Err(e) => {
            app.status_message = Some((format!("Query Error: {}", e), false));
            return;
        }
    };

    let mut data_path = Path::new("bittice/data");
    if !data_path.exists() { data_path = Path::new("data"); }
    
    let base_path = data_path.join(&payload.entity);
    let table = match Table::open(&base_path, &payload.table) {
        Ok(t) => t,
        Err(e) => {
            app.status_message = Some((format!("Table Error: {}", e), false));
            return;
        }
    };

    let fields = if payload.fields.is_empty() {
        get_base_fields(&get_indexed_fields(&payload.entity, &payload.table))
    } else {
        payload.fields
    };

    app.status_message = Some(("Running query...".to_string(), true));
    
    match table.search(&fields, &payload.filters, &crate::core::types::LogicalOp::And, &[], &payload.order_by, 100, 0) {
        Ok(result) => {
            app.search_results = Some(result);
            app.status_message = Some(("Query executed successfully".to_string(), true));
            app.results_scroll = 0;
            app.results_scroll_x = 0;
        }
        Err(e) => {
            app.status_message = Some((format!("Search Error: {}", e), false));
        }
    }
}

fn validate_json_status(app: &mut App) {
    let full_content = app.b_editor_lines.join("\n");
    let trimmed = full_content.trim();
    if trimmed.is_empty() {
        app.status_message = None;
        return;
    }
    if let Ok(_) = serde_json::from_str::<Value>(trimmed) {
        app.status_message = Some(("Valid JSON".to_string(), true));
    } else {
        app.status_message = Some(("Invalid JSON".to_string(), false));
    }
}

fn delete_selection(app: &mut App) -> bool {
    if let Some(((s_l, s_c), (e_l, e_c))) = get_sorted_selection(app.b_selection) {
        if s_l == e_l {
            app.b_editor_lines[s_l].replace_range(s_c..e_c, "");
        } else {
            let prefix = app.b_editor_lines[s_l][..s_c].to_string();
            let suffix = app.b_editor_lines[e_l][e_c..].to_string();
            app.b_editor_lines[s_l] = format!("{}{}", prefix, suffix);
            for _ in (s_l + 1)..=e_l {
                app.b_editor_lines.remove(s_l + 1);
            }
        }
        app.b_cursor = (s_l, s_c);
        app.b_selection = None;
        return true;
    }
    false
}

fn paste_and_format(app: &mut App, text: &str) {
    delete_selection(app);
    let cleaned = text.replace('“', "\"").replace('”', "\"").replace('‘', "'").replace('’', "'");
    let mut current_pos = app.b_cursor;
    
    for (i, line_text) in cleaned.lines().enumerate() {
        if i > 0 {
            if current_pos.0 < app.b_editor_lines.len() {
                let remainder = app.b_editor_lines[current_pos.0].split_off(current_pos.1);
                app.b_editor_lines.insert(current_pos.0 + 1, remainder);
                current_pos = (current_pos.0 + 1, 0);
            }
        }
        if current_pos.0 < app.b_editor_lines.len() {
            app.b_editor_lines[current_pos.0].insert_str(current_pos.1, line_text);
            current_pos.1 += line_text.len();
        }
    }
    app.b_cursor = current_pos;
    
    let full_content = app.b_editor_lines.join("\n");
    if let Ok(value) = serde_json::from_str::<Value>(&full_content) {
        if let Ok(pretty) = serde_json::to_string_pretty(&value) {
            app.b_editor_lines = pretty.lines().map(String::from).collect();
            if app.b_editor_lines.is_empty() { app.b_editor_lines.push(String::new()); }
            app.b_cursor = (0, 0);
            app.status_message = Some(("JSON formatted".to_string(), true));
        }
    }
    validate_json_status(app);
}

fn get_selected_text(app: &App) -> Option<String> {
    let ((s_l, s_c), (e_l, e_c)) = get_sorted_selection(app.b_selection)?;
    let mut selected = Vec::new();
    
    for l_idx in s_l..=e_l {
        let line = &app.b_editor_lines[l_idx];
        if s_l == e_l {
            selected.push(&line[s_c..e_c]);
        } else if l_idx == s_l {
            selected.push(&line[s_c..]);
        } else if l_idx == e_l {
            selected.push(&line[..e_c]);
        } else {
            selected.push(line);
        }
    }
    Some(selected.join("\n"))
}

pub fn handle_bittice_input(app: &mut App, event: event::Event) -> anyhow::Result<()> {
    let (width, height) = crossterm::terminal::size().unwrap_or((100, 50));
    let size = Rect::new(0, 0, width, height);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(size);
    let body_chunks = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(25), Constraint::Percentage(75)]).split(chunks[1]);
    let catalog_area = body_chunks[0];
    let right_chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body_chunks[1]);
    let editor_area = right_chunks[0];
    let results_area = right_chunks[1];

    match event {
        event::Event::Paste(text) => {
            if app.b_focused == BitticePanel::Editor {
                paste_and_format(app, &text);
            }
        }
        event::Event::Mouse(mouse) => {
            let (x, y) = (mouse.column, mouse.row);
            let pos = Position::new(x, y);
            
            match mouse.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    if catalog_area.contains(pos) {
                        app.b_focused = BitticePanel::Catalog;
                        let rel_y = y.saturating_sub(catalog_area.y + 1);
                        let clicked_idx = rel_y as usize + app.b_catalog_scroll;
                        if clicked_idx < app.b_catalog_data.len() {
                            app.b_catalog_state.select(Some(clicked_idx));
                            let mut current = 0;
                            if toggle_catalog_node(&mut app.b_catalog_nodes, clicked_idx, &mut current) {
                                app.b_catalog_data.clear();
                                flatten_catalog_nodes(&app.b_catalog_nodes, &mut app.b_catalog_data, &[]);
                            }
                        }
                    } else if editor_area.contains(pos) {
                        app.b_focused = BitticePanel::Editor;
                        let rel_y = y.saturating_sub(editor_area.y + 1);
                        let gutter_width = format!("{}", app.b_editor_lines.len()).len() as u16 + 2;
                        let rel_x = x.saturating_sub(editor_area.x + 1 + gutter_width);
                        
                        let target_y = (rel_y as usize + app.b_editor_scroll).min(app.b_editor_lines.len().saturating_sub(1));
                        let target_x = (rel_x as usize + app.b_editor_scroll_x).min(app.b_editor_lines[target_y].len());
                        
                        app.b_cursor = (target_y, target_x);
                        app.b_selection = Some(((target_y, target_x), (target_y, target_x)));
                        app.b_is_selecting = true;
                        sync_editor_scroll(app);
                    }
                    else if results_area.contains(pos) { 
                        app.b_focused = BitticePanel::Results; 
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) => {
                    if app.b_focused == BitticePanel::Editor && app.b_is_selecting {
                        if y <= editor_area.y {
                            app.b_editor_scroll = app.b_editor_scroll.saturating_sub(1);
                        } else if y >= editor_area.y + editor_area.height - 1 {
                            let max_editor = app.b_editor_lines.len().saturating_sub(editor_area.height.saturating_sub(2) as usize);
                            app.b_editor_scroll = (app.b_editor_scroll + 1).min(max_editor);
                        }

                        let rel_y = (y as i32).saturating_sub(editor_area.y as i32 + 1);
                        let gutter_width = format!("{}", app.b_editor_lines.len()).len() as i32 + 2;
                        let rel_x = (x as i32).saturating_sub(editor_area.x as i32 + 1 + gutter_width);
                        
                        let target_y = ((rel_y + app.b_editor_scroll as i32) as usize).clamp(0, app.b_editor_lines.len().saturating_sub(1));
                        let target_x = ((rel_x + app.b_editor_scroll_x as i32) as usize).clamp(0, app.b_editor_lines[target_y].len());
                        
                        app.b_cursor = (target_y, target_x);
                        if let Some((start, _)) = app.b_selection {
                            app.b_selection = Some((start, (target_y, target_x)));
                        }
                    }
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    app.b_is_selecting = false;
                    if let Some((start, end)) = app.b_selection {
                        if start == end { app.b_selection = None; }
                    }
                }
                MouseEventKind::ScrollUp => {
                    if app.b_focused == BitticePanel::Catalog && catalog_area.contains(pos) {
                        app.b_catalog_scroll = app.b_catalog_scroll.saturating_sub(3);
                        app.b_catalog_state.select(Some(app.b_catalog_scroll));
                    } else if app.b_focused == BitticePanel::Editor && editor_area.contains(pos) {
                        app.b_editor_scroll = app.b_editor_scroll.saturating_sub(3);
                    } else if app.b_focused == BitticePanel::Results && results_area.contains(pos) {
                        app.results_scroll = app.results_scroll.saturating_sub(3);
                    }
                }
                MouseEventKind::ScrollDown => {
                    if app.b_focused == BitticePanel::Catalog && catalog_area.contains(pos) {
                        let visible_h = catalog_area.height.saturating_sub(2) as usize;
                        let max_scroll = app.b_catalog_data.len().saturating_sub(visible_h);
                        if app.b_catalog_scroll < max_scroll {
                            app.b_catalog_scroll = (app.b_catalog_scroll + 3).min(max_scroll);
                            app.b_catalog_state.select(Some(app.b_catalog_scroll));
                        }
                    } else if app.b_focused == BitticePanel::Editor && editor_area.contains(pos) {
                        let visible_h = editor_area.height.saturating_sub(2) as usize;
                        let max_editor = app.b_editor_lines.len().saturating_sub(visible_h);
                        if app.b_editor_scroll < max_editor {
                            app.b_editor_scroll = (app.b_editor_scroll + 3).min(max_editor);
                        }
                    } else if app.b_focused == BitticePanel::Results && results_area.contains(pos) {
                        if let Some(results) = &app.search_results {
                            let visible_h = results_area.height.saturating_sub(4) as usize;
                            let max_scroll = results.rows.len().saturating_sub(visible_h);
                            app.results_scroll = (app.results_scroll + 3).min(max_scroll as u16);
                        }
                    }
                }
                _ => {}
            }
        }
        event::Event::Key(key) => {
            if app.b_focused == BitticePanel::Editor {
                if key.code == KeyCode::Esc { app.b_focused = BitticePanel::Catalog; }
                else { handle_native_editor(app, key); }
            } else if app.b_focused == BitticePanel::Catalog {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Err(anyhow::anyhow!("Quit")),
                    KeyCode::Up => {
                        let i = match app.b_catalog_state.selected() {
                            Some(i) => i.saturating_sub(1),
                            None => 0,
                        };
                        app.b_catalog_state.select(Some(i));
                        if i < app.b_catalog_scroll { app.b_catalog_scroll = i; }
                    }
                    KeyCode::Down => {
                        let i = match app.b_catalog_state.selected() {
                            Some(i) => (i + 1).min(app.b_catalog_data.len().saturating_sub(1)),
                            None => 0,
                        };
                        app.b_catalog_state.select(Some(i));
                        let visible_h = catalog_area.height.saturating_sub(2) as usize;
                        if i >= app.b_catalog_scroll + visible_h { app.b_catalog_scroll = i.saturating_sub(visible_h - 1); }
                    }
                    KeyCode::Char('r') => {
                        app.b_catalog_nodes = get_catalog_tree();
                        app.b_catalog_data.clear();
                        flatten_catalog_nodes(&app.b_catalog_nodes, &mut app.b_catalog_data, &[]);
                    }
                    KeyCode::Enter => {
                        if let Some(idx) = app.b_catalog_state.selected() {
                            let mut current = 0;
                            if toggle_catalog_node(&mut app.b_catalog_nodes, idx, &mut current) {
                                app.b_catalog_data.clear();
                                flatten_catalog_nodes(&app.b_catalog_nodes, &mut app.b_catalog_data, &[]);
                            }
                        }
                    }
                    KeyCode::Tab => { app.b_focused = BitticePanel::Editor; }
                    _ => {}
                }
            } else if app.b_focused == BitticePanel::Results {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Err(anyhow::anyhow!("Quit")),
                    KeyCode::Up => { app.results_scroll = app.results_scroll.saturating_sub(1); }
                    KeyCode::Down => { 
                        if let Some(results) = &app.search_results {
                            let visible_h = results_area.height.saturating_sub(4) as usize;
                            let max_scroll = results.rows.len().saturating_sub(visible_h);
                            app.results_scroll = (app.results_scroll + 1).min(max_scroll as u16);
                        }
                    }
                    KeyCode::Tab => { app.b_focused = BitticePanel::Catalog; }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_native_editor(app: &mut App, key: event::KeyEvent) {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let _shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let (mut l, mut c) = app.b_cursor;
    let mut changed = false;

    match key.code {
        KeyCode::Char('a') if ctrl => {
            let last_l = app.b_editor_lines.len().saturating_sub(1);
            let last_c = app.b_editor_lines[last_l].len();
            app.b_selection = Some(((0, 0), (last_l, last_c)));
            app.b_cursor = (last_l, last_c);
            return;
        }
        KeyCode::Char('r') if ctrl => { execute_query(app); return; }
        KeyCode::Char('c') if ctrl => {
            if let Some(text) = get_selected_text(app) {
                if let Some(ref mut cb) = app.b_clipboard { let _ = cb.set_text(text); }
            }
            return;
        }
        KeyCode::Char('v') if ctrl => {
            if let Some(ref mut cb) = app.b_clipboard {
                if let Ok(text) = cb.get_text() { paste_and_format(app, &text); changed = true; }
            }
        }
        KeyCode::Char('f') if ctrl => {
            let full_content = app.b_editor_lines.join("\n");
            if let Ok(value) = serde_json::from_str::<Value>(&full_content) {
                if let Ok(pretty) = serde_json::to_string_pretty(&value) {
                    app.b_editor_lines = pretty.lines().map(String::from).collect();
                    if app.b_editor_lines.is_empty() { app.b_editor_lines.push(String::new()); }
                    app.b_cursor = (0, 0);
                    app.status_message = Some(("Formatted".to_string(), true));
                    changed = true;
                }
            }
        }
        KeyCode::Char(ch) => {
            delete_selection(app);
            let (nl, nc) = app.b_cursor;
            app.b_editor_lines[nl].insert(nc, ch);
            app.b_cursor = (nl, nc + 1);
            changed = true;
        }
        KeyCode::Enter => {
            delete_selection(app);
            let (nl, nc) = app.b_cursor;
            let line = app.b_editor_lines[nl].clone();
            let before = &line[..nc];
            let after = &line[nc..];
            let indent_len = line.chars().take_while(|ch| *ch == ' ').count();
            
            if (before.trim_end().ends_with('{') && after.trim_start().starts_with('}')) ||
               (before.trim_end().ends_with('[') && after.trim_start().starts_with(']')) {
                app.b_editor_lines[nl] = before.to_string();
                app.b_editor_lines.insert(nl + 1, " ".repeat(indent_len + 2));
                app.b_editor_lines.insert(nl + 2, format!("{}{}", " ".repeat(indent_len), after));
                app.b_cursor = (nl + 1, indent_len + 2);
            } else {
                let mut new_indent = indent_len;
                if before.trim_end().ends_with('{') || before.trim_end().ends_with('[') {
                    new_indent += 2;
                }
                app.b_editor_lines[nl] = before.to_string();
                app.b_editor_lines.insert(nl + 1, format!("{}{}", " ".repeat(new_indent), after));
                app.b_cursor = (nl + 1, new_indent);
            }
            changed = true;
        }
        KeyCode::Backspace => {
            if !delete_selection(app) {
                if c > 0 {
                    app.b_editor_lines[l].remove(c - 1);
                    app.b_cursor = (l, c - 1);
                    changed = true;
                } else if l > 0 {
                    let current_line = app.b_editor_lines.remove(l);
                    l -= 1;
                    c = app.b_editor_lines[l].len();
                    app.b_editor_lines[l].push_str(&current_line);
                    app.b_cursor = (l, c);
                    changed = true;
                }
            } else {
                changed = true;
            }
        }
        KeyCode::Delete => {
            if !delete_selection(app) {
                if c < app.b_editor_lines[l].len() {
                    app.b_editor_lines[l].remove(c);
                    changed = true;
                } else if l < app.b_editor_lines.len() - 1 {
                    let next_line = app.b_editor_lines.remove(l + 1);
                    app.b_editor_lines[l].push_str(&next_line);
                    changed = true;
                }
            } else {
                changed = true;
            }
        }
        KeyCode::Left => {
            app.b_selection = None;
            if c > 0 { app.b_cursor = (l, c - 1); }
            else if l > 0 { l -= 1; app.b_cursor = (l, app.b_editor_lines[l].len()); }
        }
        KeyCode::Right => {
            app.b_selection = None;
            if c < app.b_editor_lines[l].len() { app.b_cursor = (l, c + 1); }
            else if l < app.b_editor_lines.len() - 1 { app.b_cursor = (l + 1, 0); }
        }
        KeyCode::Up => {
            app.b_selection = None;
            if l > 0 {
                l -= 1;
                c = c.min(app.b_editor_lines[l].len());
                app.b_cursor = (l, c);
            }
        }
        KeyCode::Down => {
            app.b_selection = None;
            if l < app.b_editor_lines.len() - 1 {
                l += 1;
                c = c.min(app.b_editor_lines[l].len());
                app.b_cursor = (l, c);
            }
        }
        KeyCode::Tab => {
            delete_selection(app);
            let (nl, nc) = app.b_cursor;
            app.b_editor_lines[nl].insert_str(nc, "  ");
            app.b_cursor = (nl, nc + 2);
            changed = true;
        }
        _ => {}
    }

    sync_editor_scroll(app);
    if changed { validate_json_status(app); }
}

pub fn sync_editor_scroll(app: &mut App) {
    let (l, _) = app.b_cursor;
    let visible_height = app.b_editor_viewport_height.saturating_sub(2) as usize;
    if visible_height == 0 { return; }

    if l < app.b_editor_scroll {
        app.b_editor_scroll = l;
    } else if l >= app.b_editor_scroll + visible_height {
        app.b_editor_scroll = l - visible_height + 1;
    }
}

pub fn bittice_ui(f: &mut Frame, app: &mut App) {
    if app.b_catalog_nodes.is_empty() {
        app.b_catalog_nodes = get_catalog_tree();
        app.b_catalog_data.clear();
        flatten_catalog_nodes(&app.b_catalog_nodes, &mut app.b_catalog_data, &[]);
    }

    let size = f.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(size);

    f.render_widget(Paragraph::new(Line::from(vec![Span::styled(" BITTICE ", Style::default().fg(DARK).bg(PRIMARY).add_modifier(Modifier::BOLD))])), chunks[0]);
    let body_chunks = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(25), Constraint::Percentage(75)]).split(chunks[1]);
    
    // --- CATALOG ---
    let catalog_block = Block::default()
        .title(" Catalog ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.b_focused == BitticePanel::Catalog { PRIMARY } else { SECONDARY }));
    
    let catalog_items: Vec<ListItem> = app.b_catalog_data.iter().map(|line| {
        let mut spans = Vec::new();
        if line.starts_with('󰆼') {
            let name = &line['󰆼'.len_utf8()..];
            spans.push(Span::styled(" 󰆼 ", Style::default().fg(DARK).bg(PRIMARY)));
            spans.push(Span::styled(format!(" {} ", name), Style::default().fg(DARK).bg(TEXT).add_modifier(Modifier::BOLD)));
        } else {
            let content_start = line.find(|c: char| c == '󰓫' || c == '󰇽').unwrap_or(0);
            let guides = &line[..content_start];
            let content = &line[content_start..];
            spans.push(Span::styled(guides, Style::default().fg(SECONDARY)));
            if content.starts_with('󰓫') {
                spans.push(Span::styled(content, Style::default().fg(Color::Rgb(222, 185, 133)))); 
            } else if content.starts_with('󰇽') {
                spans.push(Span::styled(content, Style::default().fg(TEXT))); 
            } else {
                spans.push(Span::styled(content, Style::default().fg(TEXT)));
            }
        }
        ListItem::new(Line::from(spans))
    }).collect();
    
    // Manual sync offset with our manual scroll
    *app.b_catalog_state.offset_mut() = app.b_catalog_scroll;
    
    f.render_stateful_widget(
        List::new(catalog_items).block(catalog_block),
        body_chunks[0], 
        &mut app.b_catalog_state
    );

    let right_chunks = Layout::default().direction(Direction::Vertical).constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(body_chunks[1]);

    // --- EDITOR ---
    app.b_editor_viewport_height = right_chunks[0].height;
    let editor_block = Block::default()
        .title(" Query Editor ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.b_focused == BitticePanel::Editor { PRIMARY } else { SECONDARY }));

    let inner_editor = editor_block.inner(right_chunks[0]);
    let gutter_width = format!("{}", app.b_editor_lines.len()).len() + 1;
    let visible_lines = inner_editor.height as usize;
    
    let mut editor_spans = Vec::new();
    for i in 0..visible_lines {
        let line_idx = i + app.b_editor_scroll;
        if line_idx >= app.b_editor_lines.len() { break; }
        
        let mut line_spans = Vec::new();
        line_spans.push(Span::styled(format!("{:>width$} ", line_idx + 1, width = gutter_width), Style::default().fg(SECONDARY)));
        
        let line_content = &app.b_editor_lines[line_idx];
        let is_cursor_line = app.b_cursor.0 == line_idx && app.b_focused == BitticePanel::Editor;
        
        if let Some((start, end)) = get_sorted_selection(app.b_selection) {
            let (s_l, s_c) = start;
            let (e_l, e_c) = end;
            
            for (char_idx, ch) in line_content.chars().enumerate() {
                let is_sel = if line_idx > s_l && line_idx < e_l { true }
                            else if line_idx == s_l && line_idx == e_l { char_idx >= s_c && char_idx < e_c }
                            else if line_idx == s_l { char_idx >= s_c }
                            else if line_idx == e_l { char_idx < e_c }
                            else { false };
                
                let is_cursor = is_cursor_line && app.b_cursor.1 == char_idx;
                
                let mut style = if is_sel { Style::default().bg(SELECTION) } else { Style::default().fg(TEXT) };
                if is_cursor {
                    style = Style::default().bg(Color::White).fg(Color::Black);
                } else if is_cursor_line {
                    style = style.bg(CURSOR_LINE);
                }
                line_spans.push(Span::styled(ch.to_string(), style));
            }
            if is_cursor_line && app.b_cursor.1 == line_content.len() {
                line_spans.push(Span::styled(" ", Style::default().bg(Color::White)));
            } else if line_idx >= s_l && line_idx < e_l {
                 line_spans.push(Span::styled(" ", Style::default().bg(SELECTION)));
            }
        } else {
            let style = Style::default().fg(TEXT);
            if is_cursor_line {
                let c_idx = app.b_cursor.1;
                if c_idx < line_content.len() {
                    let before = &line_content[..c_idx];
                    let cursor_char = &line_content[c_idx..c_idx+1];
                    let after = &line_content[c_idx+1..];
                    line_spans.push(Span::styled(before, style.bg(CURSOR_LINE)));
                    line_spans.push(Span::styled(cursor_char, Style::default().bg(Color::White).fg(Color::Black)));
                    line_spans.push(Span::styled(after, style.bg(CURSOR_LINE)));
                } else {
                    line_spans.push(Span::styled(line_content, style.bg(CURSOR_LINE)));
                    line_spans.push(Span::styled(" ", Style::default().bg(Color::White)));
                }
            } else {
                line_spans.push(Span::styled(line_content, style));
            }
        }
        editor_spans.push(Line::from(line_spans));
    }

    f.render_widget(Paragraph::new(editor_spans).block(editor_block), right_chunks[0]);
    
    // --- RESULTS ---
    let results_block = Block::default()
        .title(" Query Result ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if app.b_focused == BitticePanel::Results { PRIMARY } else { SECONDARY }));

    f.render_widget(&results_block, right_chunks[1]);
    let results_inner_area = results_block.inner(right_chunks[1]);

    if let Some(results) = &app.search_results {
        let results_layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Min(0)])
            .split(results_inner_area);

        let time_str = if results.execution_time_micros < 1000 { 
            format!("{}µs", results.execution_time_micros) 
        } else { 
            format!("{:.2}ms", results.execution_time_micros as f64 / 1000.0) 
        };
        let summary_text = format!("{} records found", results.total_found);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(summary_text, Style::default().fg(PRIMARY)),
                Span::styled(" | ", Style::default().fg(SECONDARY)),
                Span::styled(time_str, Style::default().fg(Color::DarkGray)),
            ])),
            results_layout[0]
        );

        if results.rows.is_empty() {
            f.render_widget(Paragraph::new(Span::styled("  No results found", Style::default().fg(Color::Red))), results_layout[1]);
        } else {
            let mut col_widths = Vec::new();
            for (i, header) in results.headers.iter().enumerate() {
                let mut max_w = header.len();
                for row in &results.rows {
                    if let Some(val) = row.get(i) {
                        if val.len() > max_w { max_w = val.len(); }
                    }
                }
                max_w = max_w.min(40).max(10);
                col_widths.push(Constraint::Length((max_w + 2) as u16));
            }

            let header_cells = results.headers.iter().map(|h| {
                Cell::from(h.as_str()).style(Style::default().fg(DARK).bg(PRIMARY).add_modifier(Modifier::BOLD))
            });
            let header = Row::new(header_cells).height(1);

            let rows = results.rows.iter().enumerate().skip(app.results_scroll as usize).map(|(i, row_data)| {
                let cells = row_data.iter().map(|c| Cell::from(c.as_str()).style(Style::default().fg(TEXT)));
                let base_row = Row::new(cells).height(1);
                if i % 2 == 0 { base_row.style(Style::default().bg(Color::Rgb(35, 35, 55))) } else { base_row }
            });

            let table = TuiTable::new(rows, col_widths)
                .header(header)
                .column_spacing(1);

            f.render_widget(table, results_layout[1]);
            
            app.last_rendered_content_height = results.rows.len() as u16;
            app.results_viewport_height = results_layout[1].height;
        }
    }

    // --- FOOTER ---
    let mut footer_text = vec![Span::styled(" ^Q ", Style::default().fg(DARK).bg(TEXT)), Span::raw(" Quit ")];
    match app.b_focused {
        BitticePanel::Catalog => {
            footer_text.extend_from_slice(&[
                Span::styled(" ^L ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Load "),
                Span::styled(" ^H ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Help "),
            ]);
        }
        BitticePanel::Editor => {
            footer_text.extend_from_slice(&[
                Span::styled(" ^R ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Run "),
                Span::styled(" ^F ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Format "),
                Span::styled(" ^O ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Open "),
                Span::styled(" ^S ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Save "),
            ]);
        }
        BitticePanel::Results => {
            footer_text.extend_from_slice(&[
                Span::styled(" ^E ", Style::default().fg(DARK).bg(PRIMARY)), Span::raw(" Export "),
            ]);
        }
    }
    footer_text.push(Span::raw(" | "));
    if let Some((msg, success)) = &app.status_message {
        footer_text.push(Span::styled(format!(" {} ", msg), Style::default().fg(DARK).bg(if *success { Color::Green } else { Color::Red })));
    } else {
        footer_text.push(Span::styled(" Ready ", Style::default().fg(TEXT)));
    }
    f.render_widget(Paragraph::new(Line::from(footer_text)).style(Style::default().bg(SECONDARY)), chunks[2]);
}

fn get_sorted_selection(sel: Option<((usize, usize), (usize, usize))>) -> Option<((usize, usize), (usize, usize))> {
    let ((s_l, s_c), (e_l, e_c)) = sel?;
    if s_l < e_l || (s_l == e_l && s_c <= e_c) {
        Some(((s_l, s_c), (e_l, e_c)))
    } else {
        Some(((e_l, e_c), (s_l, s_c)))
    }
}
