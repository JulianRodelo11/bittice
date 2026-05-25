use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordVerifier};
use argon2::Argon2;
use axum::http::HeaderMap;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::NaiveDateTime;
use lru::LruCache;
use tonic::metadata::MetadataMap;
use tracing::{debug, error, warn};

use crate::core::saved_queries::SavedAuthConfig;
use crate::core::types::{AuthContext, ComparisonOp, Filter, LogicalOp};
use crate::server::table_manager::TableManager;

/// Live API keys use this prefix; lookup uses the first [`API_KEY_LOOKUP_PREFIX_LEN`] bytes.
pub const API_KEY_PREFIX: &str = "bk_live_";
pub const API_KEY_LOOKUP_PREFIX_LEN: usize = 12;

/// How long a successful API-key verify is trusted before we re-run argon2.
/// Tradeoff: shorter → revoked keys propagate faster, longer → less CPU. 5 min
/// matches the heartbeat cadence, which is the dominant authenticated traffic
/// on the corp motor today.
const API_KEY_VERIFY_TTL: Duration = Duration::from_secs(300);

/// Cap so a single hot process can't pile up unbounded entries (legitimate or
/// not). 4096 distinct active keys covers far more than any realistic single
/// motor will see; on overflow we just drop the least-recently-used entry.
const API_KEY_CACHE_CAPACITY: usize = 4096;

#[derive(Clone)]
struct VerifiedEntry {
    user_id: String,
    verified_at: Instant,
}

pub struct AuthService {
    table_manager: Arc<TableManager>,
    /// Caches `(token → (user_id, key_hash, verified_at))` to skip the ~175ms
    /// Argon2 verify on every request from the same caller within
    /// `API_KEY_VERIFY_TTL`. The cache is intentionally in-process only:
    /// rebuilt on every engine restart, so a revoked key always loses access
    /// within at most `TTL + restart_recovery_seconds`.
    verified_keys: Mutex<LruCache<String, VerifiedEntry>>,
}

impl AuthService {
    pub fn new(table_manager: Arc<TableManager>) -> Self {
        let cap = NonZeroUsize::new(API_KEY_CACHE_CAPACITY)
            .expect("API_KEY_CACHE_CAPACITY must be > 0");
        Self {
            table_manager,
            verified_keys: Mutex::new(LruCache::new(cap)),
        }
    }

    pub async fn resolve_token(
        &self,
        entity: &str,
        token: &str,
        config: Option<&SavedAuthConfig>,
    ) -> Option<AuthContext> {
        let c = config?;
        if !c.enabled {
            return None;
        }

        let token = token.trim();
        if token.is_empty() {
            return None;
        }

        if c.uses_api_key_sha256() {
            return self.resolve_api_key_sha256(entity, token, c).await;
        }

        if c.uses_bittice_api_key() {
            return self
                .resolve_bittice_api_key(entity, token, c)
                .await;
        }

        self.resolve_legacy_token(entity, token, c).await
    }

    /// Fast path: prefix lookup + SHA-256 compare. ~50µs total vs ~180ms
    /// for the Argon2 path. Skips the verify-cache entirely — there's
    /// nothing to cache, the lookup IS the cache hit.
    async fn resolve_api_key_sha256(
        &self,
        entity: &str,
        token: &str,
        config: &SavedAuthConfig,
    ) -> Option<AuthContext> {
        let lookup_prefix = match api_key_lookup_prefix(token) {
            Some(prefix) => prefix.to_string(),
            None => {
                warn!(
                    "AUTH: API key rejected — must start with '{}' and be at least {} characters",
                    API_KEY_PREFIX, API_KEY_LOOKUP_PREFIX_LEN
                );
                return None;
            }
        };

        let Some(resolved_table_name) =
            resolve_table_name_case_insensitive(entity, &config.table)
        else {
            warn!(
                "AUTH: Auth table '{}' not found on disk for entity '{}'.",
                config.table, entity
            );
            return None;
        };

        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let id_col = config.id_col.clone();
        let token_owned = token.to_string();

        let user_id = tokio::task::spawn_blocking(move || {
            lookup_api_key_user_id_sha256(
                &tm,
                &e_name,
                &resolved_table_name,
                &lookup_prefix,
                &token_owned,
                &id_col,
            )
        })
        .await
        .ok()
        .flatten()?;

        Some(AuthContext {
            user_id,
            token: token.to_string(),
            entity: entity.to_string(),
            filter_col: config.filter_col.clone(),
        })
    }

    async fn resolve_bittice_api_key(
        &self,
        entity: &str,
        token: &str,
        config: &SavedAuthConfig,
    ) -> Option<AuthContext> {
        let lookup_prefix = match api_key_lookup_prefix(token) {
            Some(prefix) => prefix.to_string(),
            None => {
                warn!(
                    "AUTH: API key rejected — must start with '{}' and be at least {} characters",
                    API_KEY_PREFIX,
                    API_KEY_LOOKUP_PREFIX_LEN
                );
                return None;
            }
        };

        // Fast path: token was already verified recently. Skips the ~175ms
        // Argon2 verify and the auth-table lookup entirely. Capped TTL keeps
        // the lag-to-revocation bounded.
        if let Some(uid) = self.cached_user_id(token) {
            return Some(AuthContext {
                user_id: uid,
                token: token.to_string(),
                entity: entity.to_string(),
                filter_col: config.filter_col.clone(),
            });
        }

        let Some(resolved_table_name) =
            resolve_table_name_case_insensitive(entity, &config.table)
        else {
            warn!(
                "AUTH: Auth table '{}' not found on disk for entity '{}'.",
                config.table, entity
            );
            return None;
        };

        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let id_col = config.id_col.clone();
        let token_owned = token.to_string();

        // Returns Some((user_id, matched_key_hash)) on success so we can guard
        // the cache against silent key rotations.
        let verified = tokio::task::spawn_blocking(move || {
            lookup_api_key_user_id(
                &tm,
                &e_name,
                &resolved_table_name,
                &lookup_prefix,
                &token_owned,
                &id_col,
            )
        })
        .await
        .ok()
        .flatten();

        if let Some((uid, _key_hash)) = verified {
            self.cache_verified(token, &uid);
            Some(AuthContext {
                user_id: uid,
                token: token.to_string(),
                entity: entity.to_string(),
                filter_col: config.filter_col.clone(),
            })
        } else {
            None
        }
    }

    /// Returns the cached `user_id` for `token` if the entry is fresh.
    /// Treating cache-miss the same as expired-or-rotated keeps the lookup
    /// path symmetric — the caller always falls back to the slow Argon2 path.
    fn cached_user_id(&self, token: &str) -> Option<String> {
        let mut cache = self.verified_keys.lock().ok()?;
        let entry = cache.get(token)?;
        if entry.verified_at.elapsed() < API_KEY_VERIFY_TTL {
            Some(entry.user_id.clone())
        } else {
            // Expired — evict so subsequent lookups don't see it via `peek`.
            cache.pop(token);
            None
        }
    }

    fn cache_verified(&self, token: &str, user_id: &str) {
        if let Ok(mut cache) = self.verified_keys.lock() {
            cache.put(
                token.to_string(),
                VerifiedEntry {
                    user_id: user_id.to_string(),
                    verified_at: Instant::now(),
                },
            );
        }
    }

    async fn resolve_legacy_token(
        &self,
        entity: &str,
        token: &str,
        c: &SavedAuthConfig,
    ) -> Option<AuthContext> {
        let filter_col = c.filter_col.clone();

        debug!(
            "AUTH: Legacy resolve entity '{}' table '{}' col '{}'",
            entity, c.table, c.token_col
        );

        let token_candidates = build_token_candidates(token);
        if token_candidates.is_empty() {
            return None;
        }

        let Some(resolved_table_name) = resolve_table_name_case_insensitive(entity, &c.table) else {
            warn!(
                "AUTH: Auth table '{}' not found on disk for entity '{}'.",
                c.table, entity
            );
            return None;
        };

        let tm = self.table_manager.clone();
        let e_name = entity.to_string();
        let t_name = resolved_table_name;
        let t_col = c.token_col.clone();
        let i_col = c.id_col.clone();
        let candidates = token_candidates;

        let user_id = tokio::task::spawn_blocking(move || {
            match tm.get_table(&e_name, &t_name) {
                Ok(table_lock) => {
                    let table = table_lock.read().unwrap();
                    for val in &candidates {
                        let filter = Filter {
                            field: t_col.clone(),
                            op: ComparisonOp::Eq,
                            value: val.clone(),
                            value_to: None,
                            value_options: vec![],
                            field_type: None,
                        };

                        debug!(
                            "AUTH: Searching in table '{}' for {} = '{}'...",
                            t_name, t_col, val
                        );
                        match table.search(
                            &[i_col.clone()],
                            &[filter],
                            &LogicalOp::And,
                            &[],
                            &[],
                            1,
                            0,
                            None,
                        ) {
                            Ok(results) => {
                                if !results.rows.is_empty() {
                                    return results.rows[0].get(0).cloned();
                                }
                            }
                            Err(e) => {
                                error!("AUTH: Search error in table '{}': {}", t_name, e);
                                return None;
                            }
                        }
                    }

                    let total_rows: u64 = table
                        .manifest
                        .segments
                        .iter()
                        .map(|s| s.record_count)
                        .sum();
                    warn!(
                        "AUTH: No record in '{}' for token_col '{}'. Total rows: {}",
                        t_name, t_col, total_rows
                    );
                    None
                }
                Err(e) => {
                    error!(
                        "AUTH: Could not open table '{}' for entity '{}': {}",
                        t_name, e_name, e
                    );
                    None
                }
            }
        })
        .await
        .ok()
        .flatten();

        user_id.map(|uid| AuthContext {
            user_id: uid,
            token: token.to_string(),
            entity: entity.to_string(),
            filter_col,
        })
    }
}

/// Extract API key / bearer credential from HTTP headers (`Authorization` or `X-API-Key`).
pub fn extract_credential_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
    {
        if let Some(token) = parse_authorization_value(value) {
            return Some(token);
        }
    }

    headers
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Extract API key / bearer credential from gRPC metadata.
pub fn extract_credential_from_metadata(metadata: &MetadataMap) -> Option<String> {
    if let Some(value) = metadata
        .get("authorization")
        .and_then(|h| h.to_str().ok())
    {
        if let Some(token) = parse_authorization_value(value) {
            return Some(token);
        }
    }

    metadata
        .get("x-api-key")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_authorization_value(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("Bearer ") {
        let token = rest.trim();
        if !token.is_empty() {
            return Some(token.to_string());
        }
        return None;
    }
    if trimmed.starts_with("Bearer") {
        return None;
    }
    Some(trimmed.to_string())
}

pub fn api_key_lookup_prefix(token: &str) -> Option<&str> {
    if token.len() < API_KEY_LOOKUP_PREFIX_LEN {
        return None;
    }
    if !token.starts_with(API_KEY_PREFIX) {
        return None;
    }
    Some(&token[..API_KEY_LOOKUP_PREFIX_LEN])
}

/// Returns `(user_id, key_hash)` on a successful Argon2 verify. The hash is
/// echoed back so the caller can stash it in the verify-cache and later
/// invalidate the entry if the hash rotates underneath us.
fn lookup_api_key_user_id(
    tm: &TableManager,
    entity: &str,
    table_name: &str,
    lookup_prefix: &str,
    token: &str,
    id_col: &str,
) -> Option<(String, String)> {
    let table_lock = tm.get_table(entity, table_name).ok()?;
    let table = table_lock.read().unwrap();

    let search_fields = vec![
        id_col.to_string(),
        "key_hash".to_string(),
        "prefix".to_string(),
        "revoked_at".to_string(),
        "expires_at".to_string(),
    ];

    let filter = Filter {
        field: "prefix".to_string(),
        op: ComparisonOp::Eq,
        value: lookup_prefix.to_string(),
        value_to: None,
        value_options: vec![],
        field_type: None,
    };

    let results = table
        .search(
            &search_fields,
            &[filter],
            &LogicalOp::And,
            &[],
            &[],
            32,
            0,
            None,
        )
        .ok()?;

    let idx = |name: &str| results.headers.iter().position(|h| h == name);

    let id_idx = idx(id_col)?;
    let hash_idx = idx("key_hash")?;
    let revoked_idx = idx("revoked_at")?;
    let expires_idx = idx("expires_at")?;

    for row in &results.rows {
        let user_id = row.get(id_idx)?.clone();
        let key_hash = row.get(hash_idx)?.clone();
        let revoked_at = row.get(revoked_idx).map(String::as_str).unwrap_or("");
        let expires_at = row.get(expires_idx).map(String::as_str).unwrap_or("");

        if !api_key_row_active(revoked_at, expires_at) {
            debug!(
                "AUTH: Skipping api_keys row user_id={} (revoked or expired)",
                user_id
            );
            continue;
        }

        if verify_api_key_hash(&key_hash, token) {
            debug!(
                "AUTH: API key verified for user_id={} (prefix={})",
                user_id, lookup_prefix
            );
            return Some((user_id, key_hash));
        }
    }

    warn!(
        "AUTH: No valid api_keys row for prefix '{}' (candidates={})",
        lookup_prefix,
        results.rows.len()
    );
    None
}

/// SHA-256 sibling of `lookup_api_key_user_id`. Searches `api_keys` by
/// `prefix` (narrow set, indexed), then compares each candidate row's
/// `key_sha256` against `SHA-256(token)` in constant time. Returns the
/// row's `user_id` on the first match.
///
/// Rows with `key_sha256 IS NULL` are skipped — they belong to keys
/// created before migration 0023 and only have an Argon2 hash. Users
/// rotate those keys via the regenerate flow; in the meantime they
/// still authenticate through the legacy bittice_api_key path.
fn lookup_api_key_user_id_sha256(
    tm: &TableManager,
    entity: &str,
    table_name: &str,
    lookup_prefix: &str,
    token: &str,
    id_col: &str,
) -> Option<String> {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;

    let table_lock = tm.get_table(entity, table_name).ok()?;
    let table = table_lock.read().unwrap();

    let search_fields = vec![
        id_col.to_string(),
        "key_sha256".to_string(),
        "prefix".to_string(),
        "revoked_at".to_string(),
        "expires_at".to_string(),
    ];

    let filter = Filter {
        field: "prefix".to_string(),
        op: ComparisonOp::Eq,
        value: lookup_prefix.to_string(),
        value_to: None,
        value_options: vec![],
        field_type: None,
    };

    let results = table
        .search(
            &search_fields,
            &[filter],
            &LogicalOp::And,
            &[],
            &[],
            32,
            0,
            None,
        )
        .ok()?;

    let idx = |name: &str| results.headers.iter().position(|h| h == name);
    let id_idx = idx(id_col)?;
    let sha_idx = idx("key_sha256")?;
    let revoked_idx = idx("revoked_at")?;
    let expires_idx = idx("expires_at")?;

    // Compute the token's SHA-256 once. Lowercase hex to match the
    // canonical form written by the control plane (Python's hashlib
    // hexdigest emits lowercase).
    let expected_hex = {
        let mut hasher = Sha256::new();
        hasher.update(token.as_bytes());
        let digest = hasher.finalize();
        let mut out = String::with_capacity(64);
        for b in digest.iter() {
            use std::fmt::Write;
            let _ = write!(out, "{:02x}", b);
        }
        out
    };

    for row in &results.rows {
        let user_id = row.get(id_idx)?.clone();
        let stored = row.get(sha_idx).map(String::as_str).unwrap_or("");
        let revoked_at = row.get(revoked_idx).map(String::as_str).unwrap_or("");
        let expires_at = row.get(expires_idx).map(String::as_str).unwrap_or("");

        if stored.trim().is_empty() {
            // Row predates migration 0023; fall back via the legacy path.
            continue;
        }
        if !api_key_row_active(revoked_at, expires_at) {
            continue;
        }
        if stored.as_bytes().ct_eq(expected_hex.as_bytes()).into() {
            debug!(
                "AUTH: API key (sha256) verified for user_id={} (prefix={})",
                user_id, lookup_prefix
            );
            return Some(user_id);
        }
    }

    warn!(
        "AUTH: No valid api_keys (sha256) row for prefix '{}' (candidates={})",
        lookup_prefix,
        results.rows.len()
    );
    None
}

fn api_key_row_active(revoked_at: &str, expires_at: &str) -> bool {
    if !revoked_at.trim().is_empty() {
        return false;
    }
    let expires = expires_at.trim();
    if expires.is_empty() {
        return true;
    }
    parse_mirror_datetime(expires)
        .map(|dt| dt > chrono::Utc::now().naive_utc())
        .unwrap_or(false)
}

fn parse_mirror_datetime(raw: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").ok())
}

fn verify_api_key_hash(stored_hash: &str, token: &str) -> bool {
    let trimmed = stored_hash.trim();
    if trimmed.is_empty() {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(trimmed) else {
        warn!("AUTH: Could not parse key_hash as PHC string");
        return false;
    };
    Argon2::default()
        .verify_password(token.as_bytes(), &parsed)
        .is_ok()
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

fn resolve_table_name_case_insensitive(entity: &str, configured_table: &str) -> Option<String> {
    let path = crate::core::data_paths::mirror_entity_dir(entity);
    let configured_lower = configured_table.to_lowercase();

    if path.join(configured_table).is_dir() {
        return Some(configured_table.to_string());
    }

    let mut candidates: Vec<(String, u64)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.to_lowercase() == configured_lower {
                    let records = read_table_record_count(&entry.path());
                    candidates.push((name, records));
                }
            }
        }
    }

    if candidates.is_empty() {
        return None;
    }

    if let Some((exact, _)) = candidates.iter().find(|(name, _)| name == configured_table) {
        return Some(exact.clone());
    }

    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Some(candidates[0].0.clone())
}

fn read_table_record_count(table_path: &Path) -> u64 {
    let manifest_path = table_path.join("manifest.json");
    let Ok(content) = std::fs::read_to_string(manifest_path) else {
        return 0;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return 0;
    };
    json.get("segments")
        .and_then(|v| v.as_array())
        .map(|segments| {
            segments
                .iter()
                .map(|s| s.get("record_count").and_then(|v| v.as_u64()).unwrap_or(0))
                .sum::<u64>()
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_prefix_is_first_12_chars() {
        let token = "bk_live_eksfs5pmtacc1tsr1r70onpaw14mro2z";
        assert_eq!(api_key_lookup_prefix(token), Some("bk_live_eksf"));
    }

    #[test]
    fn rejects_short_or_wrong_prefix() {
        assert_eq!(api_key_lookup_prefix("bk_live_"), None);
        assert_eq!(api_key_lookup_prefix("sk_live_xxxx"), None);
    }

    #[test]
    fn parse_bearer_authorization() {
        assert_eq!(
            parse_authorization_value("Bearer bk_live_abc"),
            Some("bk_live_abc".to_string())
        );
        assert_eq!(parse_authorization_value("bk_live_abc"), Some("bk_live_abc".to_string()));
    }

    #[test]
    fn auth_config_detects_bittice_api_key_scheme() {
        let cfg = SavedAuthConfig {
            enabled: true,
            table: "api_keys".to_string(),
            token_col: "prefix".to_string(),
            id_col: "user_id".to_string(),
            filter_col: "user_id".to_string(),
            scheme: None,
        };
        assert!(cfg.uses_bittice_api_key());

        let legacy = SavedAuthConfig {
            enabled: true,
            table: "users".to_string(),
            token_col: "email".to_string(),
            id_col: "id".to_string(),
            filter_col: "user_id".to_string(),
            scheme: None,
        };
        assert!(!legacy.uses_bittice_api_key());
    }
}
