//! Shared deploy helpers (project root discovery for cloud deploy image detection).

use std::path::PathBuf;

/// Find a directory with `Cargo.toml` naming package `bittice`, walking up from cwd.
pub fn find_bittice_project_root() -> Option<PathBuf> {
    let mut p = std::env::current_dir().ok()?;
    loop {
        let cargo = p.join("Cargo.toml");
        if cargo.is_file() {
            if let Ok(m) = std::fs::read_to_string(&cargo) {
                if m.lines().any(|l| l.trim() == "name = \"bittice\"") {
                    return Some(p);
                }
            }
        }
        p = p.parent()?.to_path_buf();
    }
}
