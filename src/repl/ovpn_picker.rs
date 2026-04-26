//! Folder navigation to pick a `.ovpn` file using cliclack's `select` (same TUI as the rest of the app).
use std::io;
use std::path::{Path, PathBuf};

use cliclack::select;

/// Best-effort home directory (USERPROFILE, HOME) or current working directory.
fn start_directory() -> PathBuf {
    let from_env = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(p) = from_env {
        if let Ok(c) = std::fs::canonicalize(&p) {
            if c.is_dir() {
                return c;
            }
        } else if p.is_dir() {
            return p;
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(c) = std::fs::canonicalize(&cwd) {
            if c.is_dir() {
                return c;
            }
        }
    }
    PathBuf::from("/")
}

fn shorten_prompt(path: &Path) -> String {
    let s = path.display().to_string();
    if s.len() <= 52 {
        return s;
    }
    let tail: String = s.chars().rev().take(49).collect::<String>().chars().rev().collect();
    format!("...{}", tail)
}

/// Keeps list rows short so cliclack’s full-width wrap does not break alignment with the frame bar.
fn truncate_entry_label(body: &str) -> String {
    let s = format!("  {body}");
    const MAX: usize = 52;
    if s.chars().count() <= MAX {
        return s;
    }
    let take = MAX.saturating_sub(1);
    format!("{}…", s.chars().take(take).collect::<String>())
}

/// Lists non-hidden subdirectories and .ovpn files, sorted.
fn list_dirs_and_ovpns(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>, Option<io::Error>) {
    let mut dirs = Vec::new();
    let mut ovpns = Vec::new();
    let mut err = None;

    match std::fs::read_dir(dir) {
        Ok(rd) => {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with('.') {
                    continue;
                }
                let path = e.path();
                let Ok(ft) = e.file_type() else {
                    continue;
                };
                if ft.is_dir() {
                    dirs.push(path);
                } else if ft.is_file()
                    && path
                        .extension()
                        .and_then(|x| x.to_str())
                        .map(|e| e.eq_ignore_ascii_case("ovpn"))
                        .unwrap_or(false)
                {
                    ovpns.push(path);
                }
            }
        }
        Err(e) => err = Some(e),
    }
    dirs.sort();
    ovpns.sort();
    (dirs, ovpns, err)
}

#[derive(Clone, PartialEq, Eq)]
enum Picked {
    Up,
    EnterDir(PathBuf),
    OvpnFile(PathBuf),
    TypeInstead,
}

/// Navigate the filesystem and pick an `.ovpn` file using the same cliclack `select` list as the rest of the app.
/// - Returns `Ok(Some(path))` with the chosen file.
/// - Returns `Ok(None)` if the user chose "Type path, URL, or paste" (caller shows `input` next).
/// - Returns `Err` (often `io::ErrorKind::Interrupted` on Esc) on cancel.
pub fn browse_for_ovpn_path() -> io::Result<Option<String>> {
    let mut current = start_directory();

    loop {
        let (dirs, ovpns, read_err) = list_dirs_and_ovpns(&current);
        if let Some(ref e) = read_err {
            println!(
                "\x1b[90m│\x1b[0m  \x1b[33m▲\x1b[0m  Could not read: {e} — use Parent or text input at the list end."
            );
        }

        let prompt = format!("Pick .ovpn   [{}]", shorten_prompt(&current));

        let mut s = select(&prompt)
            .max_rows(20)
            .filter_mode();

        if current.parent().is_some() {
            s = s.item(Picked::Up, "  ⬆ Parent folder", "");
        }
        for d in &dirs {
            if let Some(name) = d.file_name() {
                let body = format!("📁 {}/", name.to_string_lossy());
                let label = truncate_entry_label(&body);
                s = s.item(Picked::EnterDir(d.clone()), label, "");
            }
        }
        for f in &ovpns {
            if let Some(name) = f.file_name() {
                let body = format!("📄 {}", name.to_string_lossy());
                let label = truncate_entry_label(&body);
                s = s.item(Picked::OvpnFile(f.clone()), label, "");
            }
        }
        s = s.item(
            Picked::TypeInstead,
            "  ✎ Path, URL, or paste (skip browse)",
            "",
        );

        let pick: Picked = s.interact()?;

        match pick {
            Picked::TypeInstead => return Ok(None),
            Picked::Up => {
                if let Some(p) = current.parent() {
                    if let Ok(c) = p.canonicalize() {
                        current = c;
                    } else {
                        current = p.to_path_buf();
                    }
                }
            }
            Picked::EnterDir(p) => {
                if p.is_dir() {
                    if let Ok(c) = p.canonicalize() {
                        current = c;
                    } else {
                        current = p;
                    }
                }
            }
            Picked::OvpnFile(p) => {
                return Ok(Some(p.to_string_lossy().into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sorts() {
        let tmp = tempfile::tempdir().expect("tmp");
        std::fs::create_dir_all(tmp.path().join("a")).unwrap();
        std::fs::write(tmp.path().join("z.ovpn"), b"x").unwrap();
        let (d, f, e) = list_dirs_and_ovpns(tmp.path());
        assert!(e.is_none());
        assert_eq!(d.len(), 1);
        assert_eq!(f.len(), 1);
    }
}
