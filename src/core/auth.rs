use std::sync::Arc;
use crate::server::table_manager::TableManager;
use crate::core::types::{AuthContext, Filter, ComparisonOp};
use crate::core::saved_queries::SavedAuthConfig;
use tracing::{debug, warn, error};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

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

        // Decodificar JWT (robusto, cayendo al token original si falla o no tiene campos esperados)
        let token_val = if token.contains('.') {
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() > 1 {
                if let Ok(decoded) = URL_SAFE_NO_PAD.decode(parts[1]) {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                        let extracted = json.get("sub").and_then(|v| v.as_str())
                            .or_else(|| json.get("username").and_then(|v| v.as_str()));
                        
                        match extracted {
                            Some(e) => {
                                debug!("AUTH: JWT payload decoded: sub/username = {}", e);
                                e.to_string()
                            },
                            None => {
                                debug!("AUTH: JWT detected but no 'sub'/'username' found. Using full token.");
                                token.to_string()
                            }
                        }
                    } else { token.to_string() }
                } else { token.to_string() }
            } else { token.to_string() }
        } else {
            token.to_string()
        };

        debug!("AUTH: Resolving identity for entity '{}' in table '{}' using col '{}' and value '{}'",
            entity, c.table, c.token_col, token_val);

        // Buscar en la tabla de usuarios
        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let t_name = c.table.clone();
        let t_col = c.token_col.clone();
        let i_col = c.id_col.clone();
        let val = token_val.clone();

        let user_id = tokio::task::spawn_blocking(move || {
            match tm.get_table(&e_name, &t_name) {
                Ok(table_lock) => {
                    let table = table_lock.read().unwrap();
                    let filter = Filter {
                        field: t_col.clone(),
                        op: ComparisonOp::Eq,
                        value: val.clone(),
                        value_options: vec![],
                        field_type: None,
                    };
                    
                    debug!("AUTH: Searching in table '{}' for {} = '{}'...", t_name, t_col, val);
                    match table.search(&[i_col], &[filter], &crate::core::types::LogicalOp::And, &[], &[], 1, 0, None) {
                        Ok(results) => {
                            if !results.rows.is_empty() {
                                let found_id = results.rows[0].get(0).cloned();
                                debug!("AUTH: Match found! user_id = {:?}", found_id);
                                found_id
                            } else {
                                let total_rows: u64 = table.manifest.segments.iter().map(|s| s.record_count).sum();
                                warn!("AUTH: No record found in table '{}' for {} = '{}'. Total rows in table: {}", 
                                    t_name, t_col, val, total_rows);
                                None
                            }
                        },
                        Err(e) => {
                            error!("AUTH: Search error in table '{}': {}", t_name, e);
                            None
                        }
                    }
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
            warn!("AUTH: Identity NOT found in table '{}' for value: {}", c.table, token_val);
            None
        }
    }
}
