use crossterm::event::{self, KeyCode};
use std::path::Path;

use crate::repl::state::{App, SearchCriteria, FilterStep, AggregationStep, FocusPanel};
use crate::repl::utils::{get_indexed_fields, get_field_values, get_order_by_fields, get_filtered_fields};

/// Inicializa el estado para la búsqueda: carga entidades y resetea paneles.
pub fn init_search(app: &mut App) {
    app.active_task = Some("Search");
    app.focus_panel = FocusPanel::Left;
    app.search_entities.clear();
    
    // LEER ENTIDADES DESDE data/
    if let Ok(entries) = std::fs::read_dir("data") {
        app.search_entities = entries.flatten()
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
    }
    app.search_entities.sort();
    
    app.left_panel_state.select(Some(0));
    // Pre-seleccionar el primer item si existe
    app.middle_panel_state.select(Some(0));
    
    // Inicializar estados
    app.search_criteria = SearchCriteria::Entity;
    app.filter_value_options = vec!["Write value".to_string()];
    app.selected_value = None;
    app.filters.clear();
    app.aggregations.clear();
    app.order_by.clear();
    app.limit = Some(100);
    app.selected_fields.clear();
    update_middle_panel_content(app);
}

pub fn handle_search_input(app: &mut App, key: event::KeyEvent) {
    if app.focus_panel == FocusPanel::Bottom {
        match key.code {
            KeyCode::Enter => {
                if !app.filter_value_input.is_empty() {
                    let val = app.filter_value_input.clone();
                    
                    match app.search_criteria {
                        SearchCriteria::Filters => {
                             if let Some(f_idx) = app.middle_panel_state.selected() {
                                if f_idx < app.filters.len() {
                                    app.filters[f_idx].value = val.clone();
                                    if !app.filters[f_idx].value_options.contains(&val) {
                                        app.filters[f_idx].value_options.push(val.clone());
                                    }
                                    app.filter_value_options = app.filters[f_idx].value_options.clone();
                                }
                            }
                        }
                        SearchCriteria::Aggregations => {
                             if let Some(f_idx) = app.middle_panel_state.selected() {
                                if f_idx < app.aggregations.len() {
                                    let agg = &mut app.aggregations[f_idx];
                                    let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);
                                    if selected_step_idx > 0 {
                                        if let Some(inner) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                                            let keys: Vec<String> = inner.keys().cloned().collect();
                                            if let Some(key) = keys.get(selected_step_idx - 1) {
                                                let num_keys = vec!["n", "top_n", "limit", "page", "min_streak"];
                                                if num_keys.contains(&key.as_str()) {
                                                    if let Ok(num) = val.parse::<u64>() {
                                                        inner.insert(key.clone(), serde_json::json!(num));
                                                    } else {
                                                        inner.insert(key.clone(), serde_json::json!(val));
                                                    }
                                                } else {
                                                    inner.insert(key.clone(), serde_json::json!(val));
                                                }
                                            }
                                        }
                                    }
                                    if !app.agg_value_options.contains(&val) {
                                        app.agg_value_options.push(val.clone());
                                    }
                                    if let Some(pos) = app.agg_value_options.iter().position(|x| x == &val) {
                                        app.extra_panel_state.select(Some(pos));
                                    }
                                }
                            }
                            app.focus_panel = FocusPanel::Extra;
                        }
                        SearchCriteria::Limit => {
                            app.limit = val.parse::<usize>().ok().map(|l| l.min(100));
                            app.focus_panel = FocusPanel::Middle;
                            app.filter_value_input.clear();
                            return;
                        }
                        _ => {}
                    }
                    
                    // Encontrar el índice del valor seleccionado para actualizar la lista en Extra
                    if let Some(idx) = app.filter_value_options.iter().position(|x| x == &val) {
                        app.extra_panel_state.select(Some(idx));
                    }
                    
                    app.filter_value_input.clear();
                }
                app.focus_panel = FocusPanel::Extra;
            },
            KeyCode::Esc => {
                app.filter_value_input.clear();
                app.focus_panel = FocusPanel::Extra;
            },
            KeyCode::Char(c) => app.filter_value_input.push(c),
            KeyCode::Backspace => { app.filter_value_input.pop(); },
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc => {
            app.active_task = None;
            // Limpiar todo el estado de búsqueda
            app.selected_entity = None;
            app.selected_table = None;
            app.selected_field = None;
            app.filters.clear();
            app.aggregations.clear();
            app.order_by.clear();
            app.limit = Some(100);
            app.selected_fields.clear();
            app.filter_value_input.clear();
            app.search_tables.clear();
            app.available_fields.clear();
            app.focus_panel = FocusPanel::Left;
            app.search_criteria = SearchCriteria::Entity;
            app.left_panel_state.select(Some(0));
            app.middle_panel_state.select(Some(0));
            app.right_panel_state.select(Some(0));
        }
        KeyCode::Right | KeyCode::Tab => match app.focus_panel {
            FocusPanel::Left => app.focus_panel = FocusPanel::Middle,
            FocusPanel::Middle => {
                if app.search_criteria == SearchCriteria::OrderBy {
                    let idx = app.middle_panel_state.selected().unwrap_or(0);
                    if idx < app.order_by.len() {
                        app.focus_panel = FocusPanel::Right;
                        app.right_panel_state.select(Some(0));
                    }
                } else if matches!(app.search_criteria, SearchCriteria::Filters | SearchCriteria::Aggregations) {
                    let idx = app.middle_panel_state.selected().unwrap_or(0);
                    let len = if app.search_criteria == SearchCriteria::Filters { app.filters.len() } else { app.aggregations.len() };
                    if idx < len {
                        app.focus_panel = FocusPanel::Right;
                    }
                }
            },
            FocusPanel::Right => {
                if matches!(app.search_criteria, SearchCriteria::Filters | SearchCriteria::Aggregations | SearchCriteria::OrderBy) {
                    app.focus_panel = FocusPanel::Extra;
                }
            },
            _ => {}
        },
        KeyCode::Left => match app.focus_panel {
            FocusPanel::Middle => app.focus_panel = FocusPanel::Left,
            FocusPanel::Right => app.focus_panel = FocusPanel::Middle,
            FocusPanel::Extra => {
                if app.search_criteria == SearchCriteria::OrderBy {
                    app.focus_panel = FocusPanel::Right;
                } else {
                    app.focus_panel = FocusPanel::Right;
                }
            },
            _ => {}
        },
        KeyCode::Up => navigate_list(app, -1),
        KeyCode::Down => navigate_list(app, 1),
        KeyCode::Enter => {
            match (app.focus_panel, app.search_criteria) {
                (FocusPanel::Middle, SearchCriteria::Entity) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let new_selection = app.search_entities.get(idx).cloned();
                        if app.selected_entity == new_selection {
                            app.selected_entity = None;
                        } else {
                            app.selected_entity = new_selection;
                        }
                        // Reset everything dependent on entity
                        app.selected_table = None;
                        app.available_fields.clear();
                        app.selected_field = None;
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Table) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let new_selection = app.search_tables.get(idx).cloned();
                        if app.selected_table == new_selection {
                            app.selected_table = None;
                        } else {
                            app.selected_table = new_selection;
                        }
                        app.selected_field = None;
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::FiltersOp) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        match idx {
                            0 => app.filters_op = "And".to_string(),
                            1 => app.filters_op = "Or".to_string(),
                            _ => {}
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Filters) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let add_new_idx = app.filters.len();
                        let delete_idx = if !app.filters.is_empty() { Some(app.filters.len() + 1) } else { None };

                        if idx == add_new_idx {
                            app.filters.push(crate::repl::state::Filter {
                                field: "?".to_string(), op: "Eq".to_string(), value: "?".to_string(),
                                value_options: vec!["Write value".to_string()],
                            });
                            app.middle_panel_state.select(Some(app.filters.len() - 1));
                            app.filter_value_options = vec!["Write value".to_string()];
                        } else if delete_idx == Some(idx) {
                            if !app.filters.is_empty() {
                                app.filters.pop();
                                let new_idx = app.filters.len().saturating_sub(1);
                                app.middle_panel_state.select(Some(new_idx));
                                if let Some(f) = app.filters.get(new_idx) {
                                    app.filter_value_options = f.value_options.clone();
                                } else {
                                    app.filter_value_options = vec!["Write value".to_string()];
                                }
                            }
                        } else {
                            if let Some(f) = app.filters.get(idx) {
                                app.filter_value_options = f.value_options.clone();
                            }
                            app.focus_panel = FocusPanel::Right;
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Aggregations) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let add_new_idx = app.aggregations.len();
                        let delete_idx = if !app.aggregations.is_empty() { Some(app.aggregations.len() + 1) } else { None };

                        if idx == add_new_idx {
                            app.aggregations.push(serde_json::json!({"TopN": {"field": "?", "n": 0}}));
                            app.middle_panel_state.select(Some(app.aggregations.len() - 1));
                        } else if delete_idx == Some(idx) {
                            if !app.aggregations.is_empty() {
                                app.aggregations.pop();
                                app.middle_panel_state.select(Some(app.aggregations.len().saturating_sub(1)));
                            }
                        } else {
                            app.focus_panel = FocusPanel::Right;
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::OrderBy) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let add_new_idx = app.order_by.len();
                        let delete_idx = if !app.order_by.is_empty() { Some(app.order_by.len() + 1) } else { None };

                        if idx == add_new_idx {
                            app.order_by.push(crate::repl::state::OrderBy {
                                field: "?".to_string(), direction: "Asc".to_string()
                            });
                            app.middle_panel_state.select(Some(app.order_by.len() - 1));
                            app.right_panel_state.select(Some(0));
                        } else if delete_idx == Some(idx) {
                            if !app.order_by.is_empty() {
                                app.order_by.pop();
                                app.middle_panel_state.select(Some(app.order_by.len().saturating_sub(1)));
                            }
                        } else {
                            app.focus_panel = FocusPanel::Right;
                            app.right_panel_state.select(Some(0));
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Limit) => {
                    app.focus_panel = FocusPanel::Bottom;
                }
                (FocusPanel::Middle, SearchCriteria::Fields) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        if let Some(field) = app.available_fields.get(idx).cloned() {
                            if app.selected_fields.contains(&field) {
                                app.selected_fields.retain(|f| f != &field);
                            } else {
                                app.selected_fields.push(field);
                            }
                        }
                    }
                }
                (FocusPanel::Right, SearchCriteria::Filters) => {
                    app.focus_panel = FocusPanel::Extra;
                }
                (FocusPanel::Right, SearchCriteria::Aggregations) => {
                    app.focus_panel = FocusPanel::Extra;
                }
                (FocusPanel::Right, SearchCriteria::OrderBy) => {
                    app.focus_panel = FocusPanel::Extra;
                }
                (FocusPanel::Extra, SearchCriteria::Filters) => {
                    match app.filter_step {
                        FilterStep::Field => {
                            if let Some(idx) = app.extra_panel_state.selected() {
                                let fields = get_filtered_fields(&app.available_fields);
                                if let Some(field) = fields.get(idx) {
                                    if let Some(f_idx) = app.middle_panel_state.selected() {
                                        if f_idx < app.filters.len() {
                                            app.filters[f_idx].field = field.clone();
                                            // ACTUALIZAR VALORES EXISTENTES
                                            if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                                                let values = get_field_values(Path::new("data"), e, t, field);
                                                app.filters[f_idx].value_options = values.clone();
                                                app.filter_value_options = values;
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        FilterStep::Op => {
                            if let Some(idx) = app.extra_panel_state.selected() {
                                let ops = vec!["Eq", "In", "Gte", "Lt"];
                                if let Some(op) = ops.get(idx) {
                                    if let Some(f_idx) = app.middle_panel_state.selected() {
                                        if f_idx < app.filters.len() {
                                            app.filters[f_idx].op = op.to_string();
                                        }
                                    }
                                }
                            }
                        },
                        FilterStep::Value => {
                            if let Some(idx) = app.extra_panel_state.selected() {
                                if let Some(val) = app.filter_value_options.get(idx) {
                                    if val == "Write value" {
                                        app.focus_panel = FocusPanel::Bottom;
                                    } else {
                                        if let Some(f_idx) = app.middle_panel_state.selected() {
                                            if f_idx < app.filters.len() {
                                                app.filters[f_idx].value = val.clone();
                                            }
                                        }
                                    }
                                }
                            }
                        },
                        _ => {}
                    }
                }
                (FocusPanel::Extra, SearchCriteria::Aggregations) => {
                    if let Some(f_idx) = app.middle_panel_state.selected() {
                        if f_idx < app.aggregations.len() {
                            let agg = &mut app.aggregations[f_idx];
                            let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);

                            if selected_step_idx == 0 {
                                // Change Type logic
                                if let Some(idx) = app.extra_panel_state.selected() {
                                    let new_type = &app.agg_type_options[idx];
                                    *agg = match new_type.as_str() {
                                        "GroupBy" => serde_json::json!({"GroupBy": {"field": "?", "operation": "Count"}}),
                                        "TopN" => serde_json::json!({"TopN": {"field": "?", "n": 10}}),
                                        "Sum" | "Avg" | "Min" | "Max" => serde_json::json!({new_type: {"field": "?"}}),
                                        "ConsecutiveBuckets" => serde_json::json!({"ConsecutiveBuckets": {"key_field": "?", "bucket_field": "?"}}),
                                        "RetentionByBucket" => serde_json::json!({"RetentionByBucket": {"key_field": "?", "bucket_field": "?"}}),
                                        "InactiveSinceBucket" => serde_json::json!({"InactiveSinceBucket": {"key_field": "?", "bucket_field": "?"}}),
                                        _ => serde_json::json!({new_type: {"field": "?"}}),
                                    };
                                }
                            } else if let Some(inner) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                                let keys: Vec<String> = inner.keys().cloned().collect();
                                if let Some(key) = keys.get(selected_step_idx - 1) {
                                    if let Some(idx) = app.extra_panel_state.selected() {
                                        match key.as_str() {
                                            "field" | "key_field" | "bucket_field" | "value_field" => {
                                                let fields = get_filtered_fields(&app.available_fields);
                                                if let Some(field) = fields.get(idx) {
                                                    inner.insert(key.clone(), serde_json::json!(field));
                                                }
                                            }
                                            "operation" => {
                                                if let Some(op) = app.agg_op_options.get(idx) {
                                                    inner.insert(key.clone(), serde_json::json!(op));
                                                }
                                            }
                                            _ => {
                                                // Manejar valores de agg_value_options (Write value + history)
                                                if let Some(val) = app.agg_value_options.get(idx) {
                                                    if val == "Write value" {
                                                        app.focus_panel = FocusPanel::Bottom;
                                                    } else {
                                                        let num_keys = vec!["n", "top_n", "limit", "page", "min_streak"];
                                                        if num_keys.contains(&key.as_str()) {
                                                            if let Ok(num) = val.parse::<u64>() {
                                                                inner.insert(key.clone(), serde_json::json!(num));
                                                            } else {
                                                                inner.insert(key.clone(), serde_json::json!(val));
                                                            }
                                                        } else {
                                                            inner.insert(key.clone(), serde_json::json!(val));
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                (FocusPanel::Extra, SearchCriteria::OrderBy) => {
                    if let Some(f_idx) = app.middle_panel_state.selected() {
                        if f_idx < app.order_by.len() {
                             if let Some(idx) = app.extra_panel_state.selected() {
                                 match app.right_panel_state.selected() {
                                     Some(0) => {
                                         let fields = get_order_by_fields(&app.available_fields);
                                         if let Some(field) = fields.get(idx) {
                                             app.order_by[f_idx].field = field.clone();
                                         }
                                     },
                                     Some(1) => app.order_by[f_idx].direction = if idx == 0 { "Asc".to_string() } else { "Desc".to_string() },
                                     _ => {}
                                 }
                             }
                        }
                    }
                }
                _ => {}
            }
        },
        _ => {}
    }
}

fn navigate_list(app: &mut App, delta: isize) {
    let (state, len) = match app.focus_panel {
        FocusPanel::Left => {
            let len = 7 + if app.filters.len() > 1 { 1 } else { 0 };
            (&mut app.left_panel_state, len)
        },
        FocusPanel::Middle => {
            let len = match app.search_criteria {
                SearchCriteria::Entity => app.search_entities.len(),
                SearchCriteria::Table => app.search_tables.len(),
                SearchCriteria::Filters => {
                     app.filters.len() + 1 + if !app.filters.is_empty() { 1 } else { 0 }
                },
                SearchCriteria::Aggregations => {
                    app.aggregations.len() + 1 + if !app.aggregations.is_empty() { 1 } else { 0 }
                },
                SearchCriteria::FiltersOp => 2,
                SearchCriteria::OrderBy => {
                    app.order_by.len() + 1 + if !app.order_by.is_empty() { 1 } else { 0 }
                },
                SearchCriteria::Limit => 1,
                SearchCriteria::Fields => app.available_fields.len(),
            };
            (&mut app.middle_panel_state, len)
        },
        FocusPanel::Right => {
            let len = match app.search_criteria {
                SearchCriteria::Filters => 3,
                SearchCriteria::OrderBy => 2,
                SearchCriteria::Aggregations => {
                    let mut count = 1; // "Change Type"
                    let current_idx = app.middle_panel_state.selected().unwrap_or(0);
                    if let Some(agg) = app.aggregations.get(current_idx) {
                        if let Some(inner) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                            count += inner.keys().count();
                        }
                    }
                    count
                },
                _ => 0,
            };
            (&mut app.right_panel_state, len)
        },
        FocusPanel::Extra => {
            let len = match app.search_criteria {
                SearchCriteria::Filters => {
                    match app.filter_step {
                        FilterStep::Field => get_filtered_fields(&app.available_fields).len(),
                        FilterStep::Value => app.filter_value_options.len(),
                        FilterStep::Op => 4, // Eq, In, Gte, Lt
                        _ => 0,
                    }
                },
                SearchCriteria::Aggregations => {
                    let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);
                    if selected_step_idx == 0 {
                        app.agg_type_options.len()
                    } else {
                        let current_idx = app.middle_panel_state.selected().unwrap_or(0);
                        if let Some(agg) = app.aggregations.get(current_idx) {
                            if let Some(inner) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                                let keys: Vec<&String> = inner.keys().collect();
                                if let Some(key) = keys.get(selected_step_idx - 1) {
                                    match key.as_str() {
                                        "field" | "key_field" | "bucket_field" | "value_field" => get_filtered_fields(&app.available_fields).len(),
                                        "operation" => app.agg_op_options.len(),
                                        _ => 1, // "Write value"
                                    }
                                } else { 0 }
                            } else { 0 }
                        } else { 0 }
                    }
                },
                SearchCriteria::OrderBy => {
                    match app.right_panel_state.selected() {
                        Some(0) => get_order_by_fields(&app.available_fields).len(),
                        Some(1) => 2, // Asc, Desc
                        _ => 0,
                    }
                },
                _ => 0,
            };
            (&mut app.extra_panel_state, len)
        }
        _ => return,
    };

    if len == 0 { if delta != 0 { return; } }
    let current = state.selected().unwrap_or(0);
    let next = if len > 0 { (current as isize + delta + len as isize) as usize % len } else { 0 };

    // Update derived state based on new selection
    match app.focus_panel {
        FocusPanel::Left => {
            let has_filters_op = app.filters.len() > 1;
            let next_criteria = match next { 
                0 => SearchCriteria::Entity, 
                1 => SearchCriteria::Table, 
                2 => SearchCriteria::Filters,
                3 if has_filters_op => SearchCriteria::FiltersOp,
                idx => {
                    let offset = if has_filters_op { 0 } else { 1 };
                    match idx + offset {
                        4 => SearchCriteria::Aggregations,
                        5 => SearchCriteria::OrderBy,
                        6 => SearchCriteria::Limit,
                        7 => SearchCriteria::Fields,
                        _ => SearchCriteria::Entity 
                    }
                }
            };
            
            // Requisitos para navegar:
            if next_criteria == SearchCriteria::Table && app.selected_entity.is_none() { return; }
            if matches!(next_criteria, SearchCriteria::Filters | SearchCriteria::FiltersOp | SearchCriteria::Aggregations | SearchCriteria::OrderBy | SearchCriteria::Limit | SearchCriteria::Fields) && app.selected_table.is_none() { return; }

            state.select(Some(next));
            app.search_criteria = next_criteria;
            
            // Inicializar estados según el criterio
            match app.search_criteria {
                SearchCriteria::Filters => {
                    app.filter_step = FilterStep::Field;
                }
                SearchCriteria::Aggregations => {
                    app.agg_value_options = vec!["Write value".to_string()];
                    app.agg_step = AggregationStep::Main;
                }
                _ => {}
            }

            app.middle_panel_state.select(Some(0));
            app.right_panel_state.select(Some(0));
            app.extra_panel_state.select(Some(0));
            update_middle_panel_content(app);
        },
        FocusPanel::Middle => {
            state.select(Some(next));
            match app.search_criteria {
                SearchCriteria::Filters => {
                    app.right_panel_state.select(Some(0));
                    app.filter_step = FilterStep::Field;
                    if let Some(f) = app.filters.get(next) { app.filter_value_options = f.value_options.clone(); }
                }
                SearchCriteria::Aggregations => {
                    app.right_panel_state.select(Some(0));
                    app.agg_step = AggregationStep::Main;
                    app.agg_value_options = vec!["Write value".to_string()];
                    // Sincronizar extra panel si es un agg existente
                    if next < app.aggregations.len() {
                        app.extra_panel_state.select(Some(0));
                    }
                }
                SearchCriteria::OrderBy => {
                    app.extra_panel_state.select(Some(0));
                }
                _ => {}
            }
        },
        FocusPanel::Right => {
            state.select(Some(next));
            if matches!(app.search_criteria, SearchCriteria::Aggregations | SearchCriteria::OrderBy) {
                 app.extra_panel_state.select(Some(0));
            } else if app.search_criteria == SearchCriteria::Filters {
                 app.filter_step = match next { 0 => FilterStep::Field, 1 => FilterStep::Op, _ => FilterStep::Value };
                 app.extra_panel_state.select(Some(0));
            }
        },
        _ => {
            state.select(Some(next));
        }
    }
}

pub fn update_middle_panel_content(app: &mut App) {
    match app.search_criteria {
        SearchCriteria::Entity => {
            // No auto-select entity here.
            // Just ensure tables are loaded for the CURRENTLY selected entity if any.
            app.search_tables.clear();
            if let Some(entity) = &app.selected_entity {
                let path = format!("data/{}", entity);
                if let Ok(tables) = std::fs::read_dir(path) {
                    app.search_tables = tables.flatten()
                        .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect();
                }
            }
            app.search_tables.sort();
        },
        SearchCriteria::Table => {
             // No auto-select table here.
             // Just ensure fields are loaded for the CURRENTLY selected entity and table.
            if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                app.available_fields = get_indexed_fields(Path::new("data"), e, t);
            } else {
                app.available_fields.clear();
            }
        },
        SearchCriteria::Filters => {
            app.right_panel_state.select(Some(0));
        },
        SearchCriteria::FiltersOp => {
             app.right_panel_state.select(None);
        },
        _ => {
            // Placeholder for other criteria
        }
    }
}
