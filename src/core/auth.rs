use std::sync::Arc;
use crate::server::table_manager::TableManager;
use crate::core::types::{AuthContext, Filter, ComparisonOp};
use crate::core::saved_queries::SavedAuthConfig;
use tracing::{debug, warn};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

pub struct AuthService {
    table_manager: Arc<TableManager>,
    default_auth_table: String,
    default_token_column: String,
    default_id_column: String,
}

impl AuthService {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        Self {
            table_manager,
            default_auth_table: "Users".to_string(),
            default_token_column: "identifier".to_string(),
            default_id_column: "id".to_string(),
        }
    }

    pub async fn resolve_token(&self, entity: &str, token: &str, config: Option<&SavedAuthConfig>) -> Option<AuthContext> {
        let auth_table;
        let token_col;
        let id_col;
        let filter_col;

        if let Some(c) = config {
            auth_table = c.table.clone();
            token_col = c.token_col.clone();
            id_col = c.id_col.clone();
            filter_col = c.filter_col.clone();
            debug!("AUTH: Using CUSTOM config: table={}, token_col={}, id_col={}", c.table, c.token_col, c.id_col);
        } else {
            auth_table = self.default_auth_table.clone();
            token_col = self.default_token_column.clone();
            id_col = self.default_id_column.clone();
            filter_col = "user_id".to_string(); // Default filter column
            debug!("AUTH: Using DEFAULT config: table={}, token_col={}, id_col={}", self.default_auth_table, self.default_token_column, self.default_id_column);
        }

        // Decodificar JWT (muy simplificado, asumiendo que el token es el valor o un JWT con 'sub')
        let token_val = if token.contains('.') {
            // Intento de decodificar el payload de un JWT sin verificar firma (solo para extraer identidad)
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() > 1 {
                if let Ok(decoded) = URL_SAFE_NO_PAD.decode(parts[1]) {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&decoded) {
                        let extracted = json.get("sub").and_then(|v| v.as_str())
                            .or_else(|| json.get("username").and_then(|v| v.as_str()))
                            .unwrap_or(token);
                        debug!("AUTH: JWT payload decoded: sub/username = {}", extracted);
                        extracted.to_string()
                    } else { token.to_string() }
                } else { token.to_string() }
            } else { token.to_string() }
        } else {
            token.to_string()
        };

        debug!("AUTH: Resolving identity for entity '{}' in table '{}' using col '{}' and value '{}'",
            entity, auth_table, token_col, token_val);

        // Buscar en la tabla de usuarios
        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let t_name = auth_table.clone();
        let t_col = token_col.clone();
        let i_col = id_col.clone();
        let val = token_val.clone();

        let user_id = tokio::task::spawn_blocking(move || {
            if let Ok(table_lock) = tm.get_table(&e_name, &t_name) {
                let table = table_lock.read().unwrap();
                let filter = Filter {
                    field: t_col,
                    op: ComparisonOp::Eq,
                    value: val,
                    value_options: vec![],
                    field_type: None,
                };
                
                let results = table.search(&[i_col], &[filter], &crate::core::types::LogicalOp::And, &[], &[], 1, 0, None).ok()?;
                if !results.rows.is_empty() {
                    return results.rows[0].get(0).cloned();
                }
            }
            None
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
            warn!("AUTH: Identity NOT found in table '{}' for value: {}", auth_table, token_val);
            None
        }
    }
}
