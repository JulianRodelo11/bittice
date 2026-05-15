use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Instant;

use tracing::{info, warn};

use crate::core::storage::exact_index::ExactIndex;
use crate::core::storage::exact_index_v3::reader::SnapshotReader;

const EXACT_IDX_MAGIC: [u8; 4] = *b"BTXI";
const EXACT_IDX_VERSION_V3: u8 = 3;

/// Result of migrating a single `exact_<field>.idx` file.
#[derive(Debug)]
pub struct MigrationResult {
    pub entity: String,
    pub table: String,
    pub field: String,
    pub path: PathBuf,
    pub format_before: String,
    pub size_before: u64,
    pub entries: usize,
    pub size_after: u64,
    pub backup_path: Option<PathBuf>,
    pub elapsed_ms: u128,
    pub skipped: bool,
    pub error: Option<String>,
}

impl MigrationResult {
    pub fn print_report(&self) {
        if self.skipped {
            println!(
                "Índice: {}/{}/{} ({})\n  Ya en v3, nada que hacer.\n",
                self.entity,
                self.table,
                self.field,
                self.path.display()
            );
            return;
        }
        if let Some(ref err) = self.error {
            println!(
                "Índice: {}/{}/{} ({})\n  ERROR: {}\n",
                self.entity,
                self.table,
                self.field,
                self.path.display(),
                err
            );
            return;
        }

        let reduction = if self.size_before > 0 {
            ((self.size_before as f64 - self.size_after as f64) / self.size_before as f64 * 100.0)
                as i64
        } else {
            0
        };

        println!(
            "Índice: {}/{}/{} ({})",
            self.entity, self.table, self.field, self.path.display()
        );
        println!("  Formato antes:   {}", self.format_before);
        println!("  Tamaño antes:    {} bytes", self.size_before);
        println!("  Entradas:        {}", self.entries);
        println!("  Formato después: v3");
        println!("  Tamaño después:  {} bytes", self.size_after);
        println!("  Reducción:       {}%", reduction);
        println!(
            "  Backup:          {}",
            self.backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "no".to_string())
        );
        println!("  Tiempo:          {} ms", self.elapsed_ms);
        println!();
    }
}

/// Detect the on-disk format of an exact index file.
/// Returns `(version, is_legacy_without_header)`.
pub fn detect_format(path: &Path) -> Result<(u8, bool), String> {
    let mut file = BufReader::new(
        fs::File::open(path).map_err(|e| format!("no se pudo abrir: {}", e))?,
    );
    let mut header = [0u8; 8];
    let n = file
        .read(&mut header)
        .map_err(|e| format!("no se pudo leer header: {}", e))?;

    if n >= 4 && header[..4] == EXACT_IDX_MAGIC {
        Ok((header[4], false))
    } else {
        Ok((1, true))
    }
}

fn format_label(version: u8, is_legacy: bool) -> String {
    if is_legacy {
        "v1 (legacy, sin header)".to_string()
    } else {
        format!("v{}", version)
    }
}

fn field_from_path(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("exact_"))
        .and_then(|n| n.strip_suffix(".idx"))
        .unwrap_or("?")
        .to_string()
}

fn backup_path_for(path: &Path) -> PathBuf {
    path.with_extension("idx.pre_v3.bak")
}

/// Migrate one `exact_<field>.idx` file to v3 in-place (with optional backup).
pub fn migrate_file(
    entity: &str,
    table: &str,
    path: &Path,
    dry_run: bool,
    keep_backup: bool,
    force: bool,
) -> MigrationResult {
    let start = Instant::now();
    let field = field_from_path(path);

    let mut result = MigrationResult {
        entity: entity.to_string(),
        table: table.to_string(),
        field,
        path: path.to_path_buf(),
        format_before: String::new(),
        size_before: 0,
        entries: 0,
        size_after: 0,
        backup_path: None,
        elapsed_ms: 0,
        skipped: false,
        error: None,
    };

    if !path.exists() {
        result.error = Some(format!("archivo no encontrado: {}", path.display()));
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    result.size_before = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    let (version, is_legacy) = match detect_format(path) {
        Ok(v) => v,
        Err(e) => {
            result.error = Some(e);
            result.elapsed_ms = start.elapsed().as_millis();
            return result;
        }
    };

    result.format_before = format_label(version, is_legacy);

    if version >= EXACT_IDX_VERSION_V3 && !force {
        result.skipped = true;
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    if version > EXACT_IDX_VERSION_V3 && !force {
        result.error = Some(format!(
            "versión {} no soportada por este binario",
            version
        ));
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    let idx = match ExactIndex::open(path) {
        Ok(i) => i,
        Err(e) => {
            result.error = Some(format!("no se pudo cargar: {}", e));
            result.elapsed_ms = start.elapsed().as_millis();
            return result;
        }
    };

    result.entries = idx.len();

    if dry_run {
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    let backup_path = backup_path_for(path);

    if let Err(e) = fs::rename(path, &backup_path) {
        result.error = Some(format!("no se pudo crear backup: {}", e));
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    let write_result = (|| -> Result<(), String> {
        let mut restored = ExactIndex::open(&backup_path)
            .map_err(|e| format!("reabrir backup para migrar: {}", e))?;
        restored
            .save(Some(path))
            .map_err(|e| format!("error escribiendo v3: {}", e))?;
        SnapshotReader::open(path).map_err(|e| format!("validación post-migración: {}", e))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::rename(&backup_path, path);
        result.error = Some(e);
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    result.size_after = fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    if keep_backup {
        result.backup_path = Some(backup_path);
    } else {
        let _ = fs::remove_file(&backup_path);
    }

    info!(
        "Migrado exact index {}/{}/{}: {} entradas, {} → {} bytes, {}ms",
        entity,
        table,
        result.field,
        result.entries,
        result.size_before,
        result.size_after,
        start.elapsed().as_millis()
    );

    result.elapsed_ms = start.elapsed().as_millis();
    result
}

/// Migrate all (or one) exact index files under `table_dir/secondary_exact/`.
pub fn migrate_table(
    entity: &str,
    table: &str,
    table_dir: &Path,
    field_filter: Option<&str>,
    dry_run: bool,
    keep_backup: bool,
    force: bool,
) -> Vec<MigrationResult> {
    let exact_dir = table_dir.join("secondary_exact");
    if !exact_dir.exists() {
        let missing = exact_dir.display().to_string();
        return vec![MigrationResult {
            entity: entity.to_string(),
            table: table.to_string(),
            field: field_filter.unwrap_or("*").to_string(),
            path: exact_dir,
            format_before: String::new(),
            size_before: 0,
            entries: 0,
            size_after: 0,
            backup_path: None,
            elapsed_ms: 0,
            skipped: false,
            error: Some(format!("directorio secondary_exact no encontrado: {}", missing)),
        }];
    }

    let mut paths: Vec<PathBuf> = match fs::read_dir(&exact_dir) {
        Ok(entries) => entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().map(|e| e == "idx").unwrap_or(false)
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("exact_"))
                        .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            return vec![MigrationResult {
                entity: entity.to_string(),
                table: table.to_string(),
                field: field_filter.unwrap_or("*").to_string(),
                path: exact_dir.clone(),
                format_before: String::new(),
                size_before: 0,
                entries: 0,
                size_after: 0,
                backup_path: None,
                elapsed_ms: 0,
                skipped: false,
                error: Some(format!("no se pudo leer {:?}: {}", exact_dir, e)),
            }];
        }
    };

    paths.sort();

    if let Some(field) = field_filter {
        let want = format!("exact_{}.idx", field);
        paths.retain(|p| p.file_name().and_then(|n| n.to_str()) == Some(want.as_str()));
        if paths.is_empty() {
            return vec![MigrationResult {
                entity: entity.to_string(),
                table: table.to_string(),
                field: field.to_string(),
                path: exact_dir.join(&want),
                format_before: String::new(),
                size_before: 0,
                entries: 0,
                size_after: 0,
                backup_path: None,
                elapsed_ms: 0,
                skipped: false,
                error: Some(format!("exact_{}.idx no encontrado", field)),
            }];
        }
    }

    if paths.is_empty() {
        return vec![MigrationResult {
            entity: entity.to_string(),
            table: table.to_string(),
            field: "*".to_string(),
            path: exact_dir,
            format_before: String::new(),
            size_before: 0,
            entries: 0,
            size_after: 0,
            backup_path: None,
            elapsed_ms: 0,
            skipped: true,
            error: None,
        }];
    }

    paths
        .into_iter()
        .map(|path| migrate_file(entity, table, &path, dry_run, keep_backup, force))
        .collect()
}

/// Discover every `exact_*.idx` under `data_root/mirror/<entity>/<table>/secondary_exact/`.
pub fn find_all_exact_indexes(data_root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut indexes = Vec::new();
    let mirror_dir = data_root.join("mirror");

    let entity_dirs: Vec<PathBuf> = if mirror_dir.exists() {
        fs::read_dir(&mirror_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect()
    } else {
        fs::read_dir(data_root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name != "profiles" && name != "mirror" && name != "vpn"
            })
            .map(|e| e.path())
            .collect()
    };

    for entity_dir in entity_dirs {
        let entity_name = entity_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if let Ok(entries) = fs::read_dir(&entity_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let table_dir = entry.path();
                let table_name = entry.file_name().to_string_lossy().into_owned();
                let exact_dir = table_dir.join("secondary_exact");
                if !exact_dir.exists() {
                    continue;
                }
                if let Ok(files) = fs::read_dir(&exact_dir) {
                    for file in files.flatten() {
                        let path = file.path();
                        if path.extension().map(|e| e == "idx").unwrap_or(false)
                            && path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.starts_with("exact_"))
                                .unwrap_or(false)
                        {
                            indexes.push((entity_name.clone(), table_name.clone(), path));
                        }
                    }
                }
            }
        }
    }

    indexes.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    indexes
}

/// Run migration for all exact index files under `data_root`.
pub fn migrate_all(
    data_root: &Path,
    dry_run: bool,
    keep_backup: bool,
    force: bool,
) -> Vec<MigrationResult> {
    let indexes = find_all_exact_indexes(data_root);
    let mut results = Vec::with_capacity(indexes.len());

    for (entity, table, path) in indexes {
        let result = migrate_file(&entity, &table, &path, dry_run, keep_backup, force);
        result.print_report();
        results.push(result);
    }

    print_summary(&results);
    results
}

fn print_summary(results: &[MigrationResult]) {
    let total = results.len();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let migrated = results.iter().filter(|r| !r.skipped && r.error.is_none()).count();
    let errors = results.iter().filter(|r| r.error.is_some()).count();
    let total_ms: u128 = results.iter().map(|r| r.elapsed_ms).sum();

    println!("=== Migración exact index completada ===");
    println!("Archivos inspeccionados:  {}", total);
    println!("Ya en v3:                 {}", skipped);
    println!("Migrados:                 {}", migrated);
    println!("Errores:                  {}", errors);
    println!("Tiempo total:             {} ms", total_ms);

    if errors > 0 {
        warn!("{} archivo(s) exact index fallaron durante la migración", errors);
    }
}

/// Print per-file reports for a table migration and aggregate summary.
pub fn print_table_results(results: &[MigrationResult]) {
    for r in results {
        r.print_report();
    }
    print_summary(results);
}
