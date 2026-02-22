use crossterm::event::{self, KeyCode};
use std::path::Path;

use crate::repl::state::{App, SearchCriteria, FilterStep, AggregationStep, FocusPanel, Filter, ComparisonOp, OrderBy, SortDirection, LogicalOp};
use crate::repl::utils::{get_indexed_fields, get_order_by_fields, get_filtered_fields, get_base_fields, get_field_values};
use crate::core::saved_queries::{SavedOperation, SavedQuery, save_operations, SavedFilter, SavedOrderBy};
use crate::core::storage::table::Table;

pub fn init_crud(app: &mut App, mode: SearchCriteria) {
    app.active_task = match mode {
        SearchCriteria::Create => Some("Create"),
        SearchCriteria::Update => Some("Update"),
        SearchCriteria::Delete => Some("Delete"),
        _ => Some("CRUD"),
    };
    app.status_message = None;
    app.focus_panel = FocusPanel::Left;
    app.search_criteria = SearchCriteria::Entity;
    
    // Read entities
    if let Ok(entries) = std::fs::read_dir("data") {
        app.search_entities = entries.flatten()
            .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
    }
    app.search_entities.sort();
    
    app.left_panel_state.select(Some(0));
    app.middle_panel_state.select(Some(0));
    
    app.crud_payload.clear();
    app.crud_target_id.clear();
    app.available_fields.clear();
    app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
    app.selected_entity = None;
    app.selected_table = None;
    if !app.saved_queries.is_empty() {
        app.saved_queries_state.select(Some(0));
    } else {
        app.saved_queries_state.select(None);
    }
}

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
    app.limit_variable = None;
    app.selected_fields.clear();
    if !app.saved_queries.is_empty() {
        app.saved_queries_state.select(Some(0));
    } else {
        app.saved_queries_state.select(None);
    }
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
                        
                        // Execute based on active task
                        match app.active_task {
                            Some("Search") => execute_search_action_with_resolved_vars(app),
                            Some("Create") | Some("Update") | Some("Delete") => execute_crud_action_with_resolved_vars(app),
                            _ => {}
                        }
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

    // 0.1 Handle Saved Queries List Overlay (PRIORITY)
    if app.show_saved_queries {
        let filtered_ops_indices: Vec<usize> = app.saved_queries.iter().enumerate().filter(|(_, op)| {
            match app.active_task {
                Some("Search") => matches!(op, SavedOperation::Read(_)),
                Some("Create") => matches!(op, SavedOperation::Insert(_)),
                Some("Update") => matches!(op, SavedOperation::Update(_)),
                Some("Delete") => matches!(op, SavedOperation::Delete(_)),
                Some("Batch") => matches!(op, SavedOperation::Read(_)),
                _ => true,
            }
        }).map(|(i, _)| i).collect();

        let filtered_len = filtered_ops_indices.len();

        match key.code {
            KeyCode::Esc => {
                app.show_saved_queries = false;
                app.is_loading_to_edit = false;
                if app.active_task == Some("Batch") {
                    app.active_task = None;
                    app.batch_selected_ops.clear();
                }
                return;
            },
            KeyCode::Up => {
                 let i = match app.saved_queries_state.selected() {
                    Some(i) => if i == 0 { filtered_len.saturating_sub(1) } else { i - 1 },
                    None => 0,
                };
                if filtered_len > 0 { app.saved_queries_state.select(Some(i)); }
                return;
            },
            KeyCode::Down => {
                let i = match app.saved_queries_state.selected() {
                    Some(i) => if i >= filtered_len.saturating_sub(1) { 0 } else { i + 1 },
                    None => 0,
                };
                if filtered_len > 0 { app.saved_queries_state.select(Some(i)); }
                return;
            },
            KeyCode::Enter => {
                if let Some(idx) = app.saved_queries_state.selected() {
                    if idx < filtered_len {
                        let original_idx = filtered_ops_indices[idx];
                        let op = app.saved_queries[original_idx].clone();
                        
                        if app.active_task == Some("Batch") {
                            let name = op.name().to_string();
                            if let Some(pos) = app.batch_selected_ops.iter().position(|x| x == &name) {
                                app.batch_selected_ops.remove(pos);
                            } else {
                                app.batch_selected_ops.push(name);
                            }
                            return;
                        }

                        match op {
                            SavedOperation::Read(query) => {
                                load_saved_query_into_app(app, &query);
                                let run_immediately = !app.is_loading_to_edit;
                                app.show_saved_queries = false;
                                app.is_loading_to_edit = false;
                                if run_immediately {
                                    execute_search_action(app);
                                }
                            }
                            SavedOperation::Insert(ins) => {
                                app.selected_entity = Some(ins.entity.clone());
                                app.selected_table = Some(ins.table.clone());
                                app.loaded_query_name = Some(ins.name.clone());
                                app.crud_payload.clear();
                                for f in &ins.expected_fields {
                                    app.crud_payload.insert(f.clone(), format!("${}", f));
                                }
                                app.show_saved_queries = false;
                                if !app.is_loading_to_edit { execute_crud_action(app); }
                            }
                            SavedOperation::Update(upd) => {
                                app.selected_entity = Some(upd.entity.clone());
                                app.selected_table = Some(upd.table.clone());
                                app.loaded_query_name = Some(upd.name.clone());
                                app.crud_payload.clear();
                                for f in &upd.allowed_fields {
                                    app.crud_payload.insert(f.clone(), format!("${}", f));
                                }
                                app.show_saved_queries = false;
                                if !app.is_loading_to_edit { execute_crud_action(app); }
                            }
                            SavedOperation::Delete(del) => {
                                app.selected_entity = Some(del.entity.clone());
                                app.selected_table = Some(del.table.clone());
                                app.loaded_query_name = Some(del.name.clone());
                                app.crud_target_id = "$id".to_string();
                                app.show_saved_queries = false;
                                if !app.is_loading_to_edit { execute_crud_action(app); }
                            }
                            SavedOperation::Batch(_) => {
                                app.show_saved_queries = false;
                            }
                        }
                    }
                }
                return;
            },
            KeyCode::Char('d') | KeyCode::Char('D') => {
                 // Delete saved query
                 if let Some(idx) = app.saved_queries_state.selected() {
                    if idx < filtered_len {
                        let original_idx = filtered_ops_indices[idx];
                        app.saved_queries.remove(original_idx);
                        let _ = save_operations(&app.saved_queries);
                        
                        let new_len = filtered_len - 1;
                        if new_len == 0 {
                            app.saved_queries_state.select(None);
                        } else {
                            let new_selection = idx.min(new_len - 1);
                            app.saved_queries_state.select(Some(new_selection));
                        }
                    }
                 }
                 return;
            }
            _ => {}
        }
        return;
    }

    // 0.2 Handle Batch specific keys (when selection list is CLOSED but task is Batch)
    if app.active_task == Some("Batch") && !app.is_saving_query {
        match key.code {
            KeyCode::Char('S') | KeyCode::Char('s') => {
                if !app.batch_selected_ops.is_empty() {
                    app.is_saving_query = true;
                    app.show_saved_queries = false;
                    app.save_query_name_input.clear();
                }
                return;
            }
            KeyCode::Esc => {
                app.active_task = None;
                app.batch_selected_ops.clear();
                return;
            }
            KeyCode::Char('L') | KeyCode::Char('l') => {
                app.show_saved_queries = true;
                if !app.saved_queries.is_empty() { app.saved_queries_state.select(Some(0)); }
                return;
            }
            _ => {}
        }
    }

    // 1. Handle Saving Query Input Overlay
    if app.is_saving_query {
        match key.code {
            KeyCode::Enter => {
                if !app.save_query_name_input.is_empty() {
                    let name = app.save_query_name_input.clone();
                    
                    let operation = match app.active_task {
                        Some("Create") => {
                            SavedOperation::Insert(crate::core::saved_queries::SavedInsert {
                                name: name.clone(),
                                entity: app.selected_entity.clone().unwrap_or_default(),
                                table: app.selected_table.clone().unwrap_or_default(),
                                expected_fields: app.crud_payload.keys().cloned().collect(),
                            })
                        },
                        Some("Update") => {
                            SavedOperation::Update(crate::core::saved_queries::SavedUpdate {
                                name: name.clone(),
                                entity: app.selected_entity.clone().unwrap_or_default(),
                                table: app.selected_table.clone().unwrap_or_default(),
                                filters: vec![], // Future: add filter support for Update templates
                                allowed_fields: app.crud_payload.keys().cloned().collect(),
                            })
                        },
                        Some("Delete") => {
                            SavedOperation::Delete(crate::core::saved_queries::SavedDelete {
                                name: name.clone(),
                                entity: app.selected_entity.clone().unwrap_or_default(),
                                table: app.selected_table.clone().unwrap_or_default(),
                                filters: vec![],
                            })
                        },
                        Some("Batch") => {
                            SavedOperation::Batch(crate::core::saved_queries::SavedBatch {
                                name: name.clone(),
                                operations: app.batch_selected_ops.clone(),
                            })
                        },
                        _ => {
                            SavedOperation::Read(SavedQuery {
                                name: name.clone(),
                                entity: app.selected_entity.clone().unwrap_or_default(),
                                table: app.selected_table.clone().unwrap_or_default(),
                                filters: app.filters.iter().map(SavedFilter::from).collect(),
                                filters_op: app.filters_op.to_string(),
                                aggregations: app.aggregations.clone(),
                                order_by: app.order_by.iter().map(SavedOrderBy::from).collect(),
                                limit: app.limit,
                                limit_param: app.limit_variable.clone(),
                                selected_fields: app.selected_fields.clone(),
                            })
                        }
                    };
                    
                    if let Some(pos) = app.saved_queries.iter().position(|op| op.name() == name) {
                        app.saved_queries[pos] = operation;
                        if let Err(e) = save_operations(&app.saved_queries) {
                            app.status_message = Some((format!("Error updating: {}", e), false));
                        } else {
                            app.status_message = Some((format!("Operation '{}' updated!", name), true));
                            app.loaded_query_name = Some(name.clone());
                        }
                    } else {
                        app.saved_queries.push(operation);
                        if let Err(e) = save_operations(&app.saved_queries) {
                            app.status_message = Some((format!("Error saving: {}", e), false));
                        } else {
                            app.status_message = Some((format!("Operation '{}' saved!", name), true));
                            app.loaded_query_name = Some(name.clone());
                        }
                    }
                    
                    app.is_saving_query = false;
                    app.save_query_name_input.clear();
                    app.batch_selected_ops.clear();
                    app.active_task = None;
                    app.show_saved_queries = false;
                }
            },
            KeyCode::Esc => {
                app.is_saving_query = false;
                app.save_query_name_input.clear();
                if app.active_task == Some("Batch") {
                    app.show_saved_queries = true;
                }
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

    if matches!(app.active_task, Some("Create") | Some("Update") | Some("Delete")) {
        handle_crud_input(app, key);
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
                            if val.starts_with('$') {
                                app.limit_variable = Some(val);
                                app.limit = None; // Will be resolved at execution
                            } else {
                                app.limit = val.parse::<usize>().ok().map(|l| l.min(1000));
                                app.limit_variable = None;
                            }
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
                            app.aggregations.push(serde_json::json!({"TopN": {"field": "?", "n": 10}}));
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
                    app.focus_panel = FocusPanel::Extra;
                    app.extra_panel_state.select(Some(0));
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
                (FocusPanel::Extra, SearchCriteria::Limit) => {
                    if let Some(idx) = app.extra_panel_state.selected() {
                        if let Some(val) = app.limit_value_options.get(idx) {
                            if val == "Write value" {
                                app.filter_value_input.clear();
                                app.focus_panel = FocusPanel::Bottom;
                            } else if val == "Variable (ask later)" {
                                app.limit_variable = Some("$limit".to_string());
                                app.filter_value_input = "$limit".to_string();
                                app.focus_panel = FocusPanel::Bottom;
                            }
                        }
                    }
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
                                         let fields = if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                                             get_order_by_fields(Path::new("data"), e, t)
                                         } else {
                                             vec![]
                                         };
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
         let mut variables = Vec::new();
         for f in &app.filters {
             if f.value.starts_with('$') { variables.push(f.value.clone()); }
         }
         if let Some(ref var) = app.limit_variable {
             if var.starts_with('$') { variables.push(var.clone()); }
         }
         
         // Detect variables in aggregations
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
         let order_by = app.order_by.clone();
         
         let fields = if app.selected_fields.is_empty() && app.aggregations.is_empty() {
             get_base_fields(&app.available_fields)
         } else {
             app.selected_fields.clone()
         };

         let entity = e.clone();
         let table_name = t.clone();

         // Position for spinner (below menu)
         let start_x = 4;
         let start_y = 9; // Menu(7) + padding

         let _ = crate::ui::spinner::run_with_spinner(
             "Executing query and fetching results...",
             start_y,
             start_x,
             |_, _| {
                 let base_path = Path::new("data").join(&entity);
                 let mut table = Table::open(&base_path, &table_name)?;
                 
                 // Execute Search on Table
                 match table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, 0) {
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
        value_options: vec!["Write value".to_string(), "Variable (ask later)".to_string()],
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
    app.limit_variable = query.limit_param.clone();
    app.selected_fields = query.selected_fields.clone();
}

fn navigate_list(app: &mut App, delta: isize) {
    let (state, len) = match app.focus_panel {
        FocusPanel::Left => {
            let len = match app.active_task {
                Some("Search") => 7 + if app.filters.len() > 1 { 1 } else { 0 },
                Some("Create") | Some("Update") | Some("Delete") => 3,
                _ => 7,
            };
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
                SearchCriteria::Create => {
                    let fields_count = app.crud_payload.len();
                    fields_count + 1 + if fields_count > 0 { 1 } else { 0 }
                },
                SearchCriteria::Update => get_base_fields(&app.available_fields).len() + 1,
                SearchCriteria::Delete => 1,
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
                SearchCriteria::Create => {
                    let current_fields: Vec<_> = app.crud_payload.keys().cloned().collect();
                    let idx = app.middle_panel_state.selected().unwrap_or(0);
                    if idx >= current_fields.len() {
                        // "+ Add Field" or "- Remove Field" selected
                        get_base_fields(&app.available_fields).len() + 1 // +1 for "Create Custom Field"
                    } else {
                        // Existing field selected: Options for value
                        1 // "Value" step
                    }
                },
                SearchCriteria::Update => 1,
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
                        Some(0) => {
                            if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                                get_order_by_fields(Path::new("data"), e, t).len()
                            } else {
                                0
                            }
                        },
                        Some(1) => 2, // Asc, Desc
                        _ => 0,
                    }
                },
                SearchCriteria::Create => {
                    let current_fields: Vec<_> = app.crud_payload.keys().cloned().collect();
                    let idx = app.middle_panel_state.selected().unwrap_or(0);
                    if idx >= current_fields.len() {
                        0 // Handled in Right panel for field selection
                    } else {
                        // Options for existing field (Write value, Variable)
                        app.filter_value_options.len()
                    }
                }
                SearchCriteria::Limit => app.limit_value_options.len(),
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
            let next_criteria = match next {
                0 => SearchCriteria::Entity,
                1 => SearchCriteria::Table,
                idx => {
                    match app.active_task {
                        Some("Search") => {
                            let has_filters_op = app.filters.len() > 1;
                            match idx {
                                2 => SearchCriteria::Filters,
                                3 if has_filters_op => SearchCriteria::FiltersOp,
                                _ => {
                                    let offset = if has_filters_op { 0 } else { 1 };
                                    match idx + offset {
                                        4 => SearchCriteria::Aggregations,
                                        5 => SearchCriteria::OrderBy,
                                        6 => SearchCriteria::Limit,
                                        7 => SearchCriteria::Fields,
                                        _ => SearchCriteria::Entity
                                    }
                                }
                            }
                        }
                        Some("Create") => SearchCriteria::Create,
                        Some("Update") => SearchCriteria::Update,
                        Some("Delete") => SearchCriteria::Delete,
                        _ => SearchCriteria::Entity
                    }
                }
            };
            
            // Requisitos para navegar:
            if next_criteria == SearchCriteria::Table && app.selected_entity.is_none() { return; }
            if matches!(next_criteria, SearchCriteria::Filters | SearchCriteria::FiltersOp | SearchCriteria::Aggregations | SearchCriteria::OrderBy | SearchCriteria::Limit | SearchCriteria::Fields | SearchCriteria::Create | SearchCriteria::Update | SearchCriteria::Delete) && app.selected_table.is_none() { return; }

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
                SearchCriteria::Create | SearchCriteria::Update => {
                    app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
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
                SearchCriteria::Create | SearchCriteria::Update => {
                    let fields = if app.search_criteria == SearchCriteria::Create {
                        let mut f: Vec<_> = app.crud_payload.keys().cloned().collect();
                        f.sort();
                        f
                    } else {
                        get_base_fields(&app.available_fields)
                    };

                    if let Some(field) = fields.get(next) {
                        let mut values = if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                            get_field_values(Path::new("data"), e, t, field)
                        } else {
                            vec![]
                        };
                        
                        let mut final_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
                        for v in values.drain(..) {
                            if !final_options.contains(&v) {
                                final_options.push(v);
                            }
                        }
                        app.filter_value_options = final_options;
                    } else {
                        app.filter_value_options = vec!["Write value".to_string(), "Variable (ask later)".to_string()];
                    }
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
         let limit = app.limit
             .or_else(|| app.limit_variable.as_ref().and_then(|v| app.variable_values.get(v)).and_then(|s| s.parse().ok()))
             .unwrap_or(100).max(1).min(1000);
         
         let mut aggregations = app.aggregations.clone();
         for agg in &mut aggregations {
             if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                 for val in obj.values_mut() {
                     if let Some(s) = val.as_str() {
                         if s.starts_with('$') {
                             if let Some(resolved) = app.variable_values.get(s) {
                                 // Try to parse as number if it's for n/limit/etc
                                 if let Ok(num) = resolved.parse::<u64>() {
                                     *val = serde_json::json!(num);
                                 } else {
                                     *val = serde_json::json!(resolved);
                                 }
                             }
                         }
                     }
                 }
             }
         }

         let order_by = app.order_by.clone();
         
         let fields = if app.selected_fields.is_empty() && app.aggregations.is_empty() {
             get_base_fields(&app.available_fields)
         } else {
             app.selected_fields.clone()
         };

         let entity = e.clone();
         let table_name = t.clone();

         let start_x = 4;
         let start_y = 9;

         let _ = crate::ui::spinner::run_with_spinner(
             "Executing query and fetching results...",
             start_y,
             start_x,
             |_, _| {
                 let base_path = Path::new("data").join(&entity);
                 let mut table = Table::open(&base_path, &table_name)?;
                 
                 match table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, 0) {
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
        let limit = app.limit
            .or_else(|| app.limit_variable.as_ref().and_then(|v| app.variable_values.get(v)).and_then(|s| s.parse().ok()))
            .unwrap_or(100).max(1).min(1000);
        
        let mut aggregations = app.aggregations.clone();
        for agg in &mut aggregations {
            if let Some(obj) = agg.as_object_mut().and_then(|o| o.values_mut().next()).and_then(|v| v.as_object_mut()) {
                for val in obj.values_mut() {
                    if let Some(s) = val.as_str() {
                        if s.starts_with('$') {
                            if let Some(resolved) = app.variable_values.get(s) {
                                if let Ok(num) = resolved.parse::<u64>() {
                                    *val = serde_json::json!(num);
                                } else {
                                    *val = serde_json::json!(resolved);
                                }
                            }
                        }
                    }
                }
            }
        }

        let order_by = app.order_by.clone();
        let offset = (app.results_page - 1) * limit;

        let fields = if app.selected_fields.is_empty() && app.aggregations.is_empty() {
            get_base_fields(&app.available_fields)
        } else {
            app.selected_fields.clone()
        };

        let entity = e.clone();
        let table_name = t.clone();

        let query_lines = crate::repl::ui::get_query_preview_lines(app).len();
        let spinner_y = (2 + 5 + 1 + 1 + 1 + query_lines) as u16;

        let _ = crate::ui::spinner::run_with_spinner(
            "Fetching next page...",
            spinner_y, 4,
            |_, _| {
                                                  let base_path = Path::new("data").join(&entity);
                                                  let mut table = Table::open(&base_path, &table_name)?;
                                                  
                                                  match table.search(&fields, &filters, &filters_op, &aggregations, &order_by, limit, offset) {
                    Ok(result) => {
                                        app.search_results = Some(result);
                                        Ok(())
                                    }
                                    Err(err) => Err(err)
                                }            }
        );
    }
}

pub fn update_middle_panel_content(app: &mut App) {
    match app.search_criteria {
        SearchCriteria::Entity => {
            // Entities are already loaded in init_search / init_crud
        },
        SearchCriteria::Table => {
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

pub fn handle_crud_input(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            if app.focus_panel == FocusPanel::Bottom {
                app.focus_panel = FocusPanel::Extra;
                app.filter_value_input.clear();
            } else {
                app.active_task = None;
                app.status_message = None;
                app.selected_entity = None;
                app.selected_table = None;
                app.crud_payload.clear();
                app.crud_target_id.clear();
                app.focus_panel = FocusPanel::Left;
                app.search_criteria = SearchCriteria::Entity;
            }
        }
        KeyCode::Up => navigate_list(app, -1),
        KeyCode::Down => navigate_list(app, 1),
        KeyCode::Left => {
            if app.focus_panel == FocusPanel::Middle {
                app.focus_panel = FocusPanel::Left;
            } else if app.focus_panel == FocusPanel::Right {
                app.focus_panel = FocusPanel::Middle;
            } else if app.focus_panel == FocusPanel::Extra {
                app.focus_panel = FocusPanel::Middle;
            } else if app.focus_panel == FocusPanel::Bottom {
                app.focus_panel = FocusPanel::Middle;
            }
        }
        KeyCode::Right | KeyCode::Tab => {
            if app.focus_panel == FocusPanel::Left {
                app.focus_panel = FocusPanel::Middle;
            } else if app.focus_panel == FocusPanel::Middle {
                if app.search_criteria == SearchCriteria::Create {
                    let current_fields_len = app.crud_payload.len();
                    let idx = app.middle_panel_state.selected().unwrap_or(0);
                    if idx >= current_fields_len {
                        app.focus_panel = FocusPanel::Right;
                        app.right_panel_state.select(Some(0));
                    } else {
                        // Skip 'Right' and go to 'Extra'
                        app.focus_panel = FocusPanel::Extra;
                        app.extra_panel_state.select(Some(0));
                    }
                } else if app.search_criteria == SearchCriteria::Update {
                    app.focus_panel = FocusPanel::Extra;
                    app.extra_panel_state.select(Some(0));
                } else if app.search_criteria == SearchCriteria::Delete {
                    app.focus_panel = FocusPanel::Bottom;
                    app.filter_value_input.clear();
                }
            } else if app.focus_panel == FocusPanel::Right {
                app.focus_panel = FocusPanel::Extra;
                app.extra_panel_state.select(Some(0));
            }
        }
        KeyCode::Char('S') => {
            app.is_saving_query = true;
            app.save_query_name_input = app.loaded_query_name.clone().unwrap_or_default();
        },
        KeyCode::Char('L') => {
            app.show_saved_queries = true;
            app.is_loading_to_edit = false;
            if !app.saved_queries.is_empty() {
                app.saved_queries_state.select(Some(0));
            }
        },
        KeyCode::Char('E') => {
            app.show_saved_queries = true;
            app.is_loading_to_edit = true;
            if !app.saved_queries.is_empty() {
                app.saved_queries_state.select(Some(0));
            }
        },
        KeyCode::Char('s') => {
            execute_crud_action(app);
        }
        KeyCode::Enter => {
            match (app.focus_panel, app.search_criteria) {
                (FocusPanel::Middle, SearchCriteria::Entity) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        app.selected_entity = app.search_entities.get(idx).cloned();
                        app.selected_table = None;
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Table) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        app.selected_table = app.search_tables.get(idx).cloned();
                        if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
                            app.available_fields = get_indexed_fields(Path::new("data"), e, t);
                            
                            app.crud_payload.clear();
                            // Only auto-populate for Update, not for Create
                            if app.active_task == Some("Update") {
                                for f in &app.available_fields {
                                    if !f.ends_with("_day") && !f.ends_with("_month") && !f.ends_with("_year") && !f.ends_with("_hour_bucket") {
                                        app.crud_payload.insert(f.clone(), String::new());
                                    }
                                }
                            }
                        }
                        update_middle_panel_content(app);
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Create) => {
                    let mut current_fields: Vec<_> = app.crud_payload.keys().cloned().collect();
                    current_fields.sort();
                    let add_field_idx = current_fields.len();
                    let remove_field_idx = if !current_fields.is_empty() { Some(current_fields.len() + 1) } else { None };

                    if let Some(idx) = app.middle_panel_state.selected() {
                        if idx == add_field_idx {
                            // User wants to add a new field from the table
                            app.focus_panel = FocusPanel::Right;
                            app.right_panel_state.select(Some(0));
                        } else if remove_field_idx == Some(idx) {
                             // User wants to remove the last added field
                             if !current_fields.is_empty() {
                                 let last = current_fields.last().unwrap();
                                 app.crud_payload.remove(last);
                             }
                        } else if idx < current_fields.len() {
                            // Edit existing selected field: Go straight to Extra
                            app.focus_panel = FocusPanel::Extra;
                            app.extra_panel_state.select(Some(0));
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Update) => {
                    let fields = get_base_fields(&app.available_fields);
                    if let Some(idx) = app.middle_panel_state.selected() {
                        if idx < fields.len() {
                            app.focus_panel = FocusPanel::Extra;
                            app.extra_panel_state.select(Some(0));
                        }
                    }
                }
                (FocusPanel::Middle, SearchCriteria::Delete) => {
                    app.focus_panel = FocusPanel::Bottom;
                    app.filter_value_input = app.crud_target_id.clone();
                }
                (FocusPanel::Right, SearchCriteria::Create) => {
                    let mut current_fields: Vec<_> = app.crud_payload.keys().cloned().collect();
                    current_fields.sort();
                    let idx = app.middle_panel_state.selected().unwrap_or(0);

                    if idx >= current_fields.len() {
                        // Selecting a field to ADD
                        if let Some(right_idx) = app.right_panel_state.selected() {
                            if right_idx == 0 {
                                // Create Custom Field
                                app.is_entering_field_name = true;
                                app.focus_panel = FocusPanel::Bottom;
                                app.filter_value_input.clear();
                            } else {
                                let available = get_base_fields(&app.available_fields);
                                if let Some(field) = available.get(right_idx - 1) {
                                    if !app.crud_payload.contains_key(field) {
                                        app.crud_payload.insert(field.clone(), String::new());
                                        
                                        // Move focus to the newly added field and prompt for value type
                                        let mut updated_fields: Vec<_> = app.crud_payload.keys().cloned().collect();
                                        updated_fields.sort();
                                        if let Some(new_pos) = updated_fields.iter().position(|f| f == field) {
                                            app.middle_panel_state.select(Some(new_pos));
                                            app.focus_panel = FocusPanel::Extra;
                                            app.right_panel_state.select(Some(0));
                                            app.extra_panel_state.select(Some(0));
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        // Selecting "Value" step for existing field
                        app.focus_panel = FocusPanel::Extra;
                        app.extra_panel_state.select(Some(0));
                    }
                }
                (FocusPanel::Right, _) => {
                    app.focus_panel = FocusPanel::Extra;
                    app.extra_panel_state.select(Some(0));
                }
                (FocusPanel::Extra, SearchCriteria::Create) | (FocusPanel::Extra, SearchCriteria::Update) => {
                    if let Some(idx) = app.extra_panel_state.selected() {
                        if let Some(val) = app.filter_value_options.get(idx).cloned() {
                             if val == "Write value" {
                                 app.focus_panel = FocusPanel::Bottom;
                                 if let Some(f_idx) = app.middle_panel_state.selected() {
                                     let fields = if app.search_criteria == SearchCriteria::Create {
                                         let mut f: Vec<_> = app.crud_payload.keys().cloned().collect();
                                         f.sort();
                                         f
                                     } else {
                                         get_base_fields(&app.available_fields)
                                     };
                                     if let Some(field) = fields.get(f_idx) {
                                         let current_val = app.crud_payload.get(field).cloned().unwrap_or_default();
                                         // If it's currently a variable, clear it for fresh "Write value"
                                         if current_val.starts_with('$') {
                                             app.filter_value_input = String::new();
                                         } else {
                                             app.filter_value_input = current_val;
                                         }
                                     }
                                 }
                             } else if val == "Variable (ask later)" {
                                 if let Some(f_idx) = app.middle_panel_state.selected() {
                                     let fields = if app.search_criteria == SearchCriteria::Create {
                                         let mut f: Vec<_> = app.crud_payload.keys().cloned().collect();
                                         f.sort();
                                         f
                                     } else {
                                         get_base_fields(&app.available_fields)
                                     };
                                     if let Some(field) = fields.get(f_idx) {
                                         let var_name = format!("${}", field);
                                         app.filter_value_input = var_name;
                                         app.focus_panel = FocusPanel::Bottom; 
                                     }
                                 }
                             } else {
                                 if let Some(f_idx) = app.middle_panel_state.selected() {
                                     let fields = if app.search_criteria == SearchCriteria::Create {
                                         let mut f: Vec<_> = app.crud_payload.keys().cloned().collect();
                                         f.sort();
                                         f
                                     } else {
                                         get_base_fields(&app.available_fields)
                                     };
                                     if let Some(field) = fields.get(f_idx) {
                                         app.crud_payload.insert(field.to_string(), val);
                                     }
                                 }
                                 app.focus_panel = FocusPanel::Middle;
                             }
                        }
                    }
                }
                (FocusPanel::Bottom, SearchCriteria::Delete) => {
                    app.crud_target_id = app.filter_value_input.clone();
                    app.focus_panel = FocusPanel::Middle;
                }
                (FocusPanel::Bottom, _) => {
                    if let Some(idx) = app.middle_panel_state.selected() {
                        let fields = if app.search_criteria == SearchCriteria::Create {
                            let mut f: Vec<_> = app.crud_payload.keys().cloned().collect();
                            f.sort();
                            f
                        } else {
                            get_base_fields(&app.available_fields)
                        };

                        if let Some(field) = fields.get(idx) {
                            let val = app.filter_value_input.clone();
                            app.crud_payload.insert(field.clone(), val.clone());
                            
                            // Add to history of options if it's a fixed value
                            if !val.starts_with('$') && !app.filter_value_options.contains(&val) {
                                app.filter_value_options.push(val);
                            }
                        }
                    }
                    // Return to Extra panel like in Search/Read mode
                    app.focus_panel = FocusPanel::Extra;
                }
                _ => {}
            }
        }
        KeyCode::Char(c) if app.focus_panel == FocusPanel::Bottom => {
            app.filter_value_input.push(c);
        }
        KeyCode::Backspace if app.focus_panel == FocusPanel::Bottom => {
            app.filter_value_input.pop();
        }
        _ => {}
    }
}

fn execute_crud_action(app: &mut App) {
    if let (Some(_e), Some(_t)) = (&app.selected_entity, &app.selected_table) {
        // Detect variables in CRUD payload
        let mut variables = Vec::new();
        for val in app.crud_payload.values() {
            if val.starts_with('$') {
                variables.push(val.clone());
            }
        }
        if app.crud_target_id.starts_with('$') {
            variables.push(app.crud_target_id.clone());
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

        execute_crud_action_with_resolved_vars(app);
    }
}

fn execute_crud_action_with_resolved_vars(app: &mut App) {
    if let (Some(e), Some(t)) = (&app.selected_entity, &app.selected_table) {
        let base_path = Path::new("data").join(e);
        let mut table = match Table::open(&base_path, t) {
            Ok(table) => table,
            Err(err) => {
                app.status_message = Some((format!("Error opening table: {}", err), false));
                return;
            }
        };

        // Resolve variables
        let mut resolved_payload = app.crud_payload.clone();
        for val in resolved_payload.values_mut() {
            if val.starts_with('$') {
                if let Some(resolved) = app.variable_values.get(val) {
                    *val = resolved.clone();
                }
            }
        }
        let mut resolved_target_id = app.crud_target_id.clone();
        if resolved_target_id.starts_with('$') {
            if let Some(resolved) = app.variable_values.get(&resolved_target_id) {
                resolved_target_id = resolved.clone();
            }
        }

        let result = match app.active_task {
            Some("Create") => {
                table.insert(resolved_payload)
            }
            Some("Update") => {
                let pk_field = if !table.manifest.primary_key.is_empty() { 
                    table.manifest.primary_key.as_str() 
                } else if resolved_payload.contains_key("id") {
                    "id"
                } else {
                    "PK" 
                };
                
                let id = resolved_payload.get(pk_field).cloned().unwrap_or_else(|| resolved_target_id.clone());
                if id.is_empty() {
                    Err(anyhow::anyhow!("Primary Key ({}) is required for Update", pk_field))
                } else {
                    table.update(&id, resolved_payload)
                }
            }
            Some("Delete") => {
                if resolved_target_id.is_empty() {
                    Err(anyhow::anyhow!("ID is required for Delete"))
                } else {
                    table.delete(&resolved_target_id)
                }
            }
            _ => Ok(()),
        };

        match result {
            Ok(_) => {
                app.status_message = Some((format!("{} operation successful!", app.active_task.unwrap_or("CRUD")), true));
                if let Err(err) = table.flush_active_segment() {
                    app.status_message = Some((format!("Error flushing: {}", err), false));
                }
            }
            Err(err) => {
                app.status_message = Some((format!("Error: {}", err), false));
            }
        }
    }
}
