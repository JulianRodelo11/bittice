use crossterm::event::{self, KeyCode};
use std::path::Path;

use crate::repl::state::{App, SearchCriteria, FilterStep, AggregationStep, FocusPanel, Filter, ComparisonOp, OrderBy, SortDirection, LogicalOp};
use crate::repl::utils::{get_indexed_fields, get_order_by_fields, get_filtered_fields, get_base_fields, get_field_values};
use crate::core::saved_queries::{SavedQuery, save_queries, SavedFilter, SavedOrderBy};

/// Inicializa el estado para la búsqueda: carga entidades y resetea paneles.
pub fn init_search(app: &mut App) {
    app.active_task = Some("Search");
    app.status_message = None; // Limpiar mensajes de carga
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
    app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
    app.selected_value = None;
    app.filters.clear();
    app.aggregations.clear();
    app.order_by.clear();
    app.variable_values.clear();
    app.loaded_query_name = None;
    app.limit = Some(100);
    app.selected_fields.clear();
    update_middle_panel_content(app);
}

pub fn handle_search_input(app: &mut App, key: event::KeyEvent) {
    // 0. Handle Variable Prompting
    if app.is_prompting_variable {
        match key.code {
            KeyCode::Enter => {
                if !app.variable_input.is_empty() {
                    app.variable_values.insert(app.current_variable.clone(), app.variable_input.clone());
                    if let Some(next_var) = app.variable_prompt_queue.pop() {
                        app.current_variable = next_var;
                        app.variable_input.clear();
                    } else {
                        app.is_prompting_variable = false;
                        app.current_variable.clear();
                        app.variable_input.clear();
                        execute_search_action_with_resolved_vars(app);
                    }
                }
            },
            KeyCode::Esc => {
                app.is_prompting_variable = false;
                app.variable_prompt_queue.clear();
                app.current_variable.clear();
                app.variable_input.clear();
            },
            KeyCode::Char(c) => app.variable_input.push(c),
            KeyCode::Backspace => { app.variable_input.pop(); },
            _ => {}
        }
        return;
    }

    // 1. Handle Saving Query Input Overlay
    if app.is_saving_query {
        match key.code {
            KeyCode::Enter => {
                if !app.save_query_name_input.is_empty() {
                    let name = app.save_query_name_input.clone();
                    // Create SavedQuery object
                    let query = SavedQuery {
                        name: name.clone(),
                        entity: app.selected_entity.clone().unwrap_or_default(),
                        table: app.selected_table.clone().unwrap_or_default(),
                        filters: app.filters.iter().map(SavedFilter::from).collect(),
                        filters_op: app.filters_op.to_string(),
                        aggregations: app.aggregations.clone(),
                        order_by: app.order_by.iter().map(SavedOrderBy::from).collect(),
                        limit: app.limit,
                        selected_fields: app.selected_fields.clone(),
                    };
                    
                    if let Some(pos) = app.saved_queries.iter().position(|q| q.name == name) {
                        app.saved_queries[pos] = query;
                        if let Err(e) = save_queries(&app.saved_queries) {
                            app.status_message = Some((format!("Error updating: {}", e), false));
                        } else {
                            app.status_message = Some((format!("Query '{}' updated!", name), true));
                            app.loaded_query_name = Some(name.clone());
                        }
                    } else {
                        app.saved_queries.push(query);
                        if let Err(e) = save_queries(&app.saved_queries) {
                            app.status_message = Some((format!("Error saving: {}", e), false));
                        } else {
                            app.status_message = Some((format!("Query '{}' saved!", name), true));
                            app.loaded_query_name = Some(name.clone());
                        }
                    }
                    
                    app.is_saving_query = false;
                    app.save_query_name_input.clear();
                }
            },
            KeyCode::Esc => {
                app.is_saving_query = false;
                app.save_query_name_input.clear();
            },
            KeyCode::Char(c) => {
                app.save_query_name_input.push(c);
            },
            KeyCode::Backspace => {
                app.save_query_name_input.pop();
            },
            _ => {}
        }
        return;
    }

    // 2. Handle Saved Queries List Overlay
    if app.show_saved_queries {
        match key.code {
            KeyCode::Esc => {
                app.show_saved_queries = false;
            },
            KeyCode::Up => {
                 let i = match app.saved_queries_state.selected() {
                    Some(i) => if i == 0 { app.saved_queries.len().saturating_sub(1) } else { i - 1 },
                    None => 0,
                };
                app.saved_queries_state.select(Some(i));
            },
            KeyCode::Down => {
                let i = match app.saved_queries_state.selected() {
                    Some(i) => if i >= app.saved_queries.len().saturating_sub(1) { 0 } else { i + 1 },
                    None => 0,
                };
                app.saved_queries_state.select(Some(i));
            },
            KeyCode::Enter => {
                if let Some(idx) = app.saved_queries_state.selected() {
                    if idx < app.saved_queries.len() {
                        let query = app.saved_queries[idx].clone();
                        load_saved_query_into_app(app, &query);
                        let run_immediately = !app.is_loading_to_edit;
                        app.show_saved_queries = false;
                        app.is_loading_to_edit = false;
                        if run_immediately {
                            execute_search_action(app);
                        }
                    }
                }
            },
            KeyCode::Char('d') => {
                 // Delete saved query
                 if let Some(idx) = app.saved_queries_state.selected() {
                    if idx < app.saved_queries.len() {
                        app.saved_queries.remove(idx);
                        let _ = save_queries(&app.saved_queries);
                        if app.saved_queries.is_empty() {
                            app.saved_queries_state.select(None);
                        } else if idx >= app.saved_queries.len() {
                            app.saved_queries_state.select(Some(app.saved_queries.len() - 1));
                        }
                    }
                 }
            }
            _ => {}
        }
        return;
    }

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
                                                let num_keys = ["n", "top_n", "limit", "page", "min_streak"];
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
        KeyCode::Up if app.search_results.is_some() => {
            app.results_scroll = app.results_scroll.saturating_sub(1);
        }
        KeyCode::Down if app.search_results.is_some() => {
            let max_scroll = app.last_rendered_content_height.saturating_sub(app.results_viewport_height);
            if app.results_scroll < max_scroll {
                 app.results_scroll = app.results_scroll.saturating_add(1);
            }
        }
        KeyCode::Right if app.search_results.is_some() => {
            app.results_scroll_x = app.results_scroll_x.saturating_add(5);
        }
        KeyCode::Left if app.search_results.is_some() => {
            app.results_scroll_x = app.results_scroll_x.saturating_sub(5);
        }
        KeyCode::Char('S') if app.search_results.is_some() => {
            app.is_saving_query = true;
            app.save_query_name_input = app.loaded_query_name.clone().unwrap_or_default();
        },
        KeyCode::Char('L') if app.search_results.is_none() => {
            app.show_saved_queries = true;
            app.is_loading_to_edit = false;
            if !app.saved_queries.is_empty() {
                app.saved_queries_state.select(Some(0));
            }
        },
        KeyCode::Char('E') if app.search_results.is_none() => {
            app.show_saved_queries = true;
            app.is_loading_to_edit = true;
            if !app.saved_queries.is_empty() {
                app.saved_queries_state.select(Some(0));
            }
        },
        KeyCode::Char('s') => {
             execute_search_action(app);
        },
        KeyCode::Char('d') if app.search_results.is_some() => {
            if let Some(results) = &app.search_results {
                let limit = app.limit.unwrap_or(100);
                if app.results_page * limit < results.total_found {
                    app.results_page += 1;
                    app.results_scroll = 0;
                    app.results_scroll_x = 0;
                    execute_paged_query(app);
                }
            }
        }
        KeyCode::Char('a') if app.search_results.is_some() => {
            if app.results_page > 1 {
                app.results_page -= 1;
                app.results_scroll = 0;
                app.results_scroll_x = 0;
                execute_paged_query(app);
            }
        }
        KeyCode::Esc => {
            if app.search_results.is_some() {
                app.search_results = None;
                app.results_scroll = 0;
                app.results_scroll_x = 0;
                return;
            }
            app.active_task = None;
            // Limpiar todo el estado de búsqueda
            app.selected_entity = None;
            app.selected_table = None;
            app.selected_field = None;
            app.filters.clear();
            app.aggregations.clear();
            app.order_by.clear();
            app.variable_values.clear();
            app.loaded_query_name = None;
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
                app.focus_panel = FocusPanel::Right;
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
                            0 => app.filters_op = crate::repl::state::LogicalOp::And,
                            1 => app.filters_op = crate::repl::state::LogicalOp::Or,
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
                                field: "?".to_string(), op: crate::repl::state::ComparisonOp::Eq, value: "?".to_string(),
                                value_options: vec!["Write value".to_string(), "Variable (ask later)".to_string()],
                            });
                            app.middle_panel_state.select(Some(app.filters.len() - 1));
                            app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
                        } else if delete_idx == Some(idx) {
                            if !app.filters.is_empty() {
                                app.filters.pop();
                                let new_idx = app.filters.len().saturating_sub(1);
                                app.middle_panel_state.select(Some(new_idx));
                                if let Some(f) = app.filters.get(new_idx) {
                                    app.filter_value_options = f.value_options.clone();
                                } else {
                                    app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
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
                                field: "?".to_string(), direction: crate::repl::state::SortDirection::Asc
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
                        let fields = get_base_fields(&app.available_fields);
                        if let Some(field) = fields.get(idx).cloned() {
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
                                            let mut values = if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                                                get_field_values(Path::new("data"), e, t, field)
                                            } else {
                                                vec!["Write value".to_string(), "Variable (ask later)".to_string()]
                                            };
                                            if !values.contains(&"Variable (ask later)".to_string()) {
                                                values.push("Variable (ask later)".to_string());
                                            }
                                            app.filters[f_idx].value_options = values.clone();
                                            app.filter_value_options = values;
                                        }
                                    }
                                }
                            }
                        },
                        FilterStep::Op => {
                            if let Some(idx) = app.extra_panel_state.selected() {
                                let ops = [crate::repl::state::ComparisonOp::Eq,
                                    crate::repl::state::ComparisonOp::Ne,
                                    crate::repl::state::ComparisonOp::In,
                                    crate::repl::state::ComparisonOp::Like,
                                    crate::repl::state::ComparisonOp::Gt,
                                    crate::repl::state::ComparisonOp::Gte,
                                    crate::repl::state::ComparisonOp::Lt,
                                    crate::repl::state::ComparisonOp::Lte];
                                if let Some(op) = ops.get(idx) {
                                    if let Some(f_idx) = app.middle_panel_state.selected() {
                                        if f_idx < app.filters.len() {
                                            app.filters[f_idx].op = op.clone();
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
                                    } else if val == "Variable (ask later)" {
                                        if let Some(f_idx) = app.middle_panel_state.selected() {
                                            if f_idx < app.filters.len() {
                                                let var_name = format!("${}", app.filters[f_idx].field);
                                                app.filters[f_idx].value = var_name.clone();
                                                app.focus_panel = FocusPanel::Bottom;
                                                app.filter_value_input = var_name;
                                            }
                                        }
                                    } else if let Some(f_idx) = app.middle_panel_state.selected() {
                                        if f_idx < app.filters.len() {
                                            app.filters[f_idx].value = val.clone();
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
                            let agg_type_str = app.aggregations[f_idx].as_object()
                                .and_then(|o| o.keys().next())
                                .map(|s| s.to_string())
                                .unwrap_or_else(|| "var".to_string());

                            let agg = &mut app.aggregations[f_idx];
                            let selected_step_idx = app.right_panel_state.selected().unwrap_or(0);

                            if selected_step_idx == 0 {
                                // Change Type logic
                                if let Some(idx) = app.extra_panel_state.selected() {
                                    let new_type = &app.agg_type_options[idx];
                                    *agg = match new_type.as_str() {
                                        "GroupBy" => serde_json::json!({"GroupBy": {"field": "?", "operation": "Count"}}),
                                        "TopN" => serde_json::json!({"TopN": {"field": "?", "n": 10}}),
                                        "Sum" => serde_json::json!({"Sum": {"field": "?", "expression": "?"}}),
                                        "Avg" | "Min" | "Max" => serde_json::json!({new_type: {"field": "?"}}),
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
                                                    } else if val == "Variable (ask later)" {
                                                        // Use Aggregation Type as variable name base (e.g. $TopN, $Sum)
                                                        let var_name = format!("${}", agg_type_str);

                                                        inner.insert(key.clone(), serde_json::json!(var_name));
                                                        app.focus_panel = FocusPanel::Bottom;
                                                        app.filter_value_input = var_name;
                                                    } else {
                                                        let num_keys = ["n", "top_n", "limit", "page", "min_streak"];
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
                                     Some(1) => app.order_by[f_idx].direction = if idx == 0 { crate::repl::state::SortDirection::Asc } else { crate::repl::state::SortDirection::Desc },
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

fn execute_search_action(app: &mut App) {
    if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
         // Detect variables
         let mut variables = Vec::new();
         for f in &app.filters {
             if f.value.starts_with('$') { variables.push(f.value.clone()); }
         }
         for agg in &app.aggregations {
             if let Some(obj) = agg.as_object().and_then(|o| o.values().next()).and_then(|v| v.as_object()) {
                 for val in obj.values() {
                     if let Some(s) = val.as_str() {
                         if s.starts_with('$') { variables.push(s.to_string()); }
                     }
                 }
             }
         }
         variables.sort();
         variables.dedup();

         if !variables.is_empty() {
             app.variable_prompt_queue = variables;
             app.is_prompting_variable = true;
             app.variable_input.clear();
             if let Some(var) = app.variable_prompt_queue.pop() {
                 app.current_variable = var;
             }
             return;
         }

         app.status_message = None; // LIMPIAR MENSAJE DE CARGA AQUÍ
         app.results_scroll = 0;
         app.results_scroll_x = 0;
         app.results_page = 1;
         let filters = app.filters.clone();
         let filters_op = app.filters_op;
         let limit = app.limit.unwrap_or(100).max(1);
         let aggregations = app.aggregations.clone();
         let order_by = app.order_by.iter().map(|o| (o.field.clone(), o.direction)).collect::<Vec<_>>();
         
         let fields = if app.selected_fields.is_empty() {
             // Filter out derived fields (_day, _month, _hour_bucket)
             app.available_fields.iter()
                 .filter(|f| {
                     let s = f.trim();
                     !s.ends_with("_day") && !s.ends_with("_month") && !s.ends_with("_year") && !s.ends_with("_hour_bucket")
                 })
                 .cloned()
                 .collect()
         } else {
             app.selected_fields.clone()
         };

         let entity = e.clone();
         let table = t.clone();

         // Position for spinner (below menu)
         let start_x = 4;
         let start_y = 9; // Menu(7) + padding

         let _ = crate::ui::spinner::run_with_spinner(
             "Executing query and fetching results...",
             start_y,
             start_x,
             |_, _| {
                 match crate::core::query::execute_query(&entity, &table, &fields, &filters, &filters_op, &aggregations, &order_by, limit, 0, &mut app.query_cache) {
                     Ok(result) => {
                         app.search_results = Some(result);
                         Ok(())
                     }
                     Err(err) => Err(err)
                 }
             }
         );
    }
}

fn load_saved_query_into_app(app: &mut App, query: &SavedQuery) {
    app.selected_entity = Some(query.entity.clone());
    app.selected_table = Some(query.table.clone());
    app.loaded_query_name = Some(query.name.clone());
    
    // Explicitly load available fields regardless of search_criteria
    if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
        app.available_fields = get_indexed_fields(Path::new("data"), e, t);
    }
    
    // Update other panel content (like table list if we are in Entity view)
    update_middle_panel_content(app);

    app.filters = query.filters.iter().map(|sf| Filter {
        field: sf.field.clone(),
        op: ComparisonOp::from_str(&sf.op),
        value: sf.value.clone(),
        value_options: vec!["Write value".to_string(), "Variable (ask later)".to_string()], // We could reload these if needed
    }).collect();
    
    app.filters_op = match query.filters_op.as_str() {
        "Or" => LogicalOp::Or,
        _ => LogicalOp::And,
    };
    
    app.aggregations = query.aggregations.clone();
    
    app.order_by = query.order_by.iter().map(|so| OrderBy {
        field: so.field.clone(),
        direction: if so.direction == "Desc" { SortDirection::Desc } else { SortDirection::Asc },
    }).collect();
    
    app.limit = query.limit;
    app.selected_fields = query.selected_fields.clone();
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
                SearchCriteria::Fields => get_base_fields(&app.available_fields).len(),
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
                        FilterStep::Op => 8, // Eq, Ne, In, Like, Gt, Gte, Lt, Lte
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
                                        _ => app.agg_value_options.len(),
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

    if len == 0 && delta != 0 { return; }
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
                    app.agg_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
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
                    app.agg_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
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

fn execute_search_action_with_resolved_vars(app: &mut App) {
    if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
         app.status_message = None;
         app.results_scroll = 0;
         app.results_scroll_x = 0;
         app.results_page = 1;
         
         let mut filters = app.filters.clone();
         for f in &mut filters {
             if f.value.starts_with('$') {
                 if let Some(resolved) = app.variable_values.get(&f.value) {
                     f.value = resolved.clone();
                 }
             }
         }

         let filters_op = app.filters_op;
         let limit = app.limit.unwrap_or(100).max(1);
         
         let mut aggregations = app.aggregations.clone();
         for agg in &mut aggregations {
             if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                 for val in obj.values_mut() {
                     if let Some(s) = val.as_str() {
                         if s.starts_with('$') {
                             if let Some(resolved) = app.variable_values.get(s) {
                                 *val = serde_json::json!(resolved);
                             }
                         }
                     }
                 }
             }
         }

         let order_by = app.order_by.iter().map(|o| (o.field.clone(), o.direction)).collect::<Vec<_>>();
         
         let fields = if app.selected_fields.is_empty() {
             app.available_fields.iter()
                 .filter(|f| {
                     let s = f.trim();
                     !s.ends_with("_day") && !s.ends_with("_month") && !s.ends_with("_year") && !s.ends_with("_hour_bucket")
                 })
                 .cloned()
                 .collect()
         } else {
             app.selected_fields.clone()
         };

         let entity = e.clone();
         let table = t.clone();

         let start_x = 4;
         let start_y = 9;

         let _ = crate::ui::spinner::run_with_spinner(
             "Executing query and fetching results...",
             start_y,
             start_x,
             |_, _| {
                 match crate::core::query::execute_query(&entity, &table, &fields, &filters, &filters_op, &aggregations, &order_by, limit, 0, &mut app.query_cache) {
                     Ok(result) => {
                         app.search_results = Some(result);
                         Ok(())
                     }
                     Err(err) => Err(err)
                 }
             }
         );
    }
}

fn execute_paged_query(app: &mut App) {
    if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
        let mut filters = app.filters.clone();
        for f in &mut filters {
            if f.value.starts_with('$') {
                if let Some(resolved) = app.variable_values.get(&f.value) {
                    f.value = resolved.clone();
                }
            }
        }

        let filters_op = app.filters_op;
        let limit = app.limit.unwrap_or(100).max(1);
        
        let mut aggregations = app.aggregations.clone();
        for agg in &mut aggregations {
            if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                for val in obj.values_mut() {
                    if let Some(s) = val.as_str() {
                        if s.starts_with('$') {
                            if let Some(resolved) = app.variable_values.get(s) {
                                *val = serde_json::json!(resolved);
                            }
                        }
                    }
                }
            }
        }

        let order_by = app.order_by.iter().map(|o| (o.field.clone(), o.direction)).collect::<Vec<_>>();
        let offset = (app.results_page - 1) * limit;

        let fields = if app.selected_fields.is_empty() {
            app.available_fields.iter()
                .filter(|f| {
                    let s = f.trim();
                    !s.ends_with("_day") && !s.ends_with("_month") && !s.ends_with("_year") && !s.ends_with("_hour_bucket")
                })
                .cloned()
                .collect()
        } else {
            app.selected_fields.clone()
        };

        let entity = e.clone();
        let table = t.clone();

        let query_lines = crate::repl::ui::get_query_preview_lines(app).len();
        let spinner_y = (2 + 5 + 1 + 1 + 1 + query_lines) as u16;

        let _ = crate::ui::spinner::run_with_spinner(
            "Fetching next page...",
            spinner_y, 4,
            |_, _| {
                match crate::core::query::execute_query(&entity, &table, &fields, &filters, &filters_op, &aggregations, &order_by, limit, offset, &mut app.query_cache) {
                    Ok(result) => {
                        app.search_results = Some(result);
                        Ok(())
                    }
                    Err(err) => Err(err)
                }
            }
        );
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
