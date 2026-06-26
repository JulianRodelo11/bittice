//! Environment flags for background warm and query table open modes.

pub fn warm_indices_only_enabled() -> bool {
    match std::env::var("BITTICE_WARM_INDICES_ONLY") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}

/// When `BITTICE_WARM_INDICES_ONLY=1`, column data prefetch is off unless this is set.
pub fn warm_prefetch_enabled() -> bool {
    if warm_indices_only_enabled() {
        return match std::env::var("BITTICE_WARM_PREFETCH") {
            Ok(v) => {
                let t = v.trim().to_ascii_lowercase();
                matches!(t.as_str(), "1" | "true" | "yes" | "on")
            }
            Err(_) => false,
        };
    }
    true
}

pub fn query_open_lazy_enabled() -> bool {
    match std::env::var("BITTICE_QUERY_OPEN_LAZY") {
        Ok(v) => {
            let t = v.trim().to_ascii_lowercase();
            matches!(t.as_str(), "1" | "true" | "yes" | "on")
        }
        Err(_) => false,
    }
}
