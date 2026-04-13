use std::sync::Arc;
use crate::server::table_manager::TableManager;
use crate::core::types::{AuthContext, Filter, ComparisonOp};
use crate::core::saved_queries::SavedAuthConfig;
use tracing::{debug, warn, error};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::path::Path;

pub struct AuthService {
    table_manager: Arc<TableManager>,
}

impl AuthService {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        Self {
            table_manager,
        }
    }

    pub async fn resolve_token(&self, entity: &str, token: &str, config: Option<&SavedAuthConfig>) -> Option<AuthContext> {
        let c = config?;
        if !c.enabled {
            return None;
        }

        let filter_col = c.filter_col.clone();

        debug!("AUTH: Resolving identity for entity '{}' in table '{}' using col '{}'",
            entity, c.table, c.token_col);

        let token_candidates = build_token_candidates(token);
        if token_candidates.is_empty() {
            return None;
        }

        let resolved_table_name = resolve_table_name_case_insensitive(entity, &c.table);
        debug!(
            "AUTH: Using resolved auth table '{}' (configured '{}'), {} token candidate(s)",
            resolved_table_name,
            c.table,
            token_candidates.len()
        );

        // Buscar en la tabla de usuarios
        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let t_name = resolved_table_name;
        let t_col = c.token_col.clone();
        let i_col = c.id_col.clone();
        let candidates = token_candidates.clone();

        let user_id = tokio::task::spawn_blocking(move || {
            match tm.get_table(&e_name, &t_name) {
                Ok(table_lock) => {
                    let table = table_lock.read().unwrap();
                    for val in &candidates {
                        let filter = Filter {
                            field: t_col.clone(),
                            op: ComparisonOp::Eq,
                            value: val.clone(),
                            value_options: vec![],
                            field_type: None,
                        };

                        debug!("AUTH: Searching in table '{}' for {} = '{}'...", t_name, t_col, val);
                        match table.search(&[i_col.clone()], &[filter], &crate::core::types::LogicalOp::And, &[], &[], 1, 0, None) {
                            Ok(results) => {
                                if !results.rows.is_empty() {
                                    let found_id = results.rows[0].get(0).cloned();
                                    debug!("AUTH: Match found with candidate '{}'. user_id = {:?}", val, found_id);
                                    return found_id;
                                }
                            },
                            Err(e) => {
                                error!("AUTH: Search error in table '{}': {}", t_name, e);
                                return None;
                            }
                        }
                    }

                    let total_rows: u64 = table.manifest.segments.iter().map(|s| s.record_count).sum();
                    warn!(
                        "AUTH: No record found in table '{}' for any token candidate in column '{}'. Total rows in table: {}",
                        t_name,
                        t_col,
                        total_rows
                    );
                    None
                },
                Err(e) => {
                    error!("AUTH: Could not open table '{}' for entity '{}': {}", t_name, e_name, e);
                    None
                }
            }
        }).await.ok().flatten();

        if let Some(uid) = user_id {
            debug!("AUTH: Identity found! Internal user_id: {}", uid);
            Some(AuthContext {
                user_id: uid,
                token: token.to_string(),
                entity: entity.to_string(),
                filter_col,
            })
        } else {
            warn!("AUTH: Identity NOT found in table '{}'", c.table);
            None
        }
    }
}

fn build_token_candidates(token: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    if !token.trim().is_empty() {
        candidates.push(token.to_string());
    }

    if token.contains('.') {
        let parts: Vec<&str> = token.split('.').collect();
        if parts.len() > 1 {
            if let Ok(decoded) = URL_SAFE_NO_PAD.decode(parts[1]) {
                if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                    let claims = ["sub", "username", "email", "user_id", "id"];
                    for key in claims {
                        if let Some(v) = json.get(key).and_then(|v| v.as_str()) {
                            if !v.trim().is_empty() {
                                candidates.push(v.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn resolve_table_name_case_insensitive(entity: &str, configured_table: &str) -> String {
    let path = Path::new("data").join(entity);
    let configured_lower = configured_table.to_lowercase();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.to_lowercase() == configured_lower {
                    return name;
                }
            }
        }
    }
    configured_table.to_string()
}
