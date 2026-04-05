use std::sync::Arc;
use serde_json;
use crate::server::table_manager::TableManager;
use crate::core::types::{Filter, ComparisonOp, LogicalOp, AuthContext};

use crate::core::saved_queries::SavedAuthConfig;

pub struct AuthService {
    table_manager: Arc<TableManager>,
    default_auth_table: String,
    default_token_column: String,
    default_id_column: String,
    default_filter_column: String,
}

impl AuthService {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        Self {
            table_manager,
            default_auth_table: std::env::var("BITTICE_AUTH_TABLE").unwrap_or_else(|_| "auth_sessions".to_string()),
            default_token_column: std::env::var("BITTICE_AUTH_TOKEN_COL").unwrap_or_else(|_| "token".to_string()),
            default_id_column: std::env::var("BITTICE_AUTH_ID_COL").unwrap_or_else(|_| "user_id".to_string()),
            default_filter_column: std::env::var("BITTICE_AUTH_FILTER_COL").unwrap_or_else(|_| "owner_id".to_string()),
        }
    }

    /// Resuelve un token a un AuthContext buscando en la tabla configurada.
    pub async fn resolve_token(&self, entity: &str, token: &str, config: Option<&SavedAuthConfig>) -> Option<AuthContext> {
        let table_manager = self.table_manager.clone();
        
        // Resolve configuration (Dynamic vs Default)
        let (auth_table, token_col, id_col, filter_col) = if let Some(c) = config {
            println!("  \x1b[35m[DEBUG-AUTH]\x1b[0m Using CUSTOM config: table={}, token_col={}, id_col={}", c.table, c.token_col, c.id_col);
            (c.table.clone(), c.token_col.clone(), c.id_col.clone(), c.filter_col.clone())
        } else {
            println!("  \x1b[35m[DEBUG-AUTH]\x1b[0m Using DEFAULT config: table={}, token_col={}, id_col={}", self.default_auth_table, self.default_token_column, self.default_id_column);
            (self.default_auth_table.clone(), self.default_token_column.clone(), self.default_id_column.clone(), self.default_filter_column.clone())
        };

        // --- JWT DECODING ---
        // Intentamos extraer el identificador real si es un JWT
        let identifier = if token.contains('.') {
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() == 3 {
                // El payload es la segunda parte
                if let Ok(payload_bytes) = decode_base64_url(parts[1]) {
                    if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) {
                        // Buscamos 'sub' o 'username'
                        let extracted = json.get("sub")
                            .or_else(|| json.get("username"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| token.to_string());
                        
                        println!("  \x1b[34m[AUTH]\x1b[0m JWT payload decoded: sub/username = {}", extracted);
                        extracted
                    } else { token.to_string() }
                } else { token.to_string() }
            } else { token.to_string() }
        } else { token.to_string() };

        let token_val = identifier;
        let entity_val = entity.to_string();
        let auth_table_for_log = auth_table.clone();

        // Ejecutamos la búsqueda en la tabla de autenticación
        let result = tokio::task::spawn_blocking(move || {
            println!("  \x1b[34m[AUTH]\x1b[0m Resolving identity for entity '{}' in table '{}' using col '{}' and value '{}'", 
                entity_val, auth_table_for_log, token_col, token_val);
            
            let table_arc = table_manager.get_table(&entity_val, &auth_table).ok()?;
            let table = table_arc.read().ok()?;
            
            let filters = vec![Filter {
                field: token_col.clone(),
                op: ComparisonOp::Eq,
                value: token_val.clone(),
                value_options: vec![],
                field_type: None,
            }];

            let query_result = table.search(
                &[id_col.clone()], 
                &filters, 
                &LogicalOp::And, 
                &[], 
                &[], 
                1, 
                0,
                None // auth_context
            ).ok()?;

            if !query_result.rows.is_empty() {
                let user_id = query_result.rows[0].get(0).cloned();
                println!("  \x1b[32m[AUTH]\x1b[0m Identity found! Internal user_id: {:?}", user_id);
                user_id
            } else {
                println!("  \x1b[31m[AUTH]\x1b[0m Identity NOT found in table '{}' for value: {}", auth_table, token_val);
                None
            }
        }).await.ok().flatten();

        result.map(|user_id| AuthContext {
            user_id,
            token: token.to_string(),
            entity: entity.to_string(),
            filter_col, // Guardamos la columna por la que filtraremos
        })
    }
}

/// Decodificador manual de Base64URL para JWT
fn decode_base64_url(input: &str) -> Result<Vec<u8>, &'static str> {
    // 1. Reemplazar caracteres Base64URL por Base64 estándar
    let base64 = input.replace('-', "+").replace('_', "/");
    
    // 2. Añadir padding si es necesario
    let padding = match base64.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    let base64_with_padding = format!("{}{}", base64, padding);
    
    // 3. Decodificar manualmente los 6 bits a bytes (versión simplificada pero efectiva para JSON)
    // Para simplificar, usamos serde_json::from_str o intentamos un decode rudimentario
    // Pero espera, Rust no tiene base64 nativo. Vamos a usar un truco con una cadena de bits 
    // o simplemente llamar a una utilidad si estuviera disponible.
    // Dado que Bittice NO tiene base64 en Cargo.toml, implementamos un decode minimalista:
    
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut map = [0u8; 256];
    for (i, &c) in ALPHABET.iter().enumerate() {
        map[c as usize] = i as u8;
    }
    
    let bytes = base64_with_padding.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == b'=' { break; }
        
        let n = (u32::from(map[bytes[i] as usize]) << 18)
              | (u32::from(map[bytes[i+1] as usize]) << 12)
              | (u32::from(map[bytes[i+2] as usize]) << 6)
              | u32::from(map[bytes[i+3] as usize]);
        
        result.push((n >> 16) as u8);
        if bytes[i+2] != b'=' { result.push((n >> 8) as u8); }
        if bytes[i+3] != b'=' { result.push(n as u8); }
        
        i += 4;
    }
    
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_base64_url() {
        // Payload típico: {"sub":"1234567890","name":"John Doe","iat":1516239022}
        let input = "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ";
        let decoded = decode_base64_url(input).expect("Should decode");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("Should be valid JSON");
        
        assert_eq!(json["sub"], "1234567890");
        assert_eq!(json["name"], "John Doe");
    }

    #[test]
    fn test_decode_base64_url_with_padding() {
        // "any car..." -> base64url sin padding
        let input = "YW55IGNhcm5hbCBwbGVhc3VyZS4";
        let decoded = decode_base64_url(input).expect("Should decode");
        assert_eq!(String::from_utf8_lossy(&decoded), "any carnal pleasure.");
    }
}
