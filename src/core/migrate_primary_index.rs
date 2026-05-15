use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::core::storage::canonical::canonical_bytes;
use crate::core::storage::primary_index::PrimaryIndex;
use crate::core::storage::primary_index_io;
use xxhash_rust::xxh3::xxh3_128;

const PRIMARY_IDX_MAGIC: [u8; 4] = *b"BTPI";
const PRIMARY_IDX_VERSION_V2: u8 = 2;

/// Result of migrating a single table.
#[derive(Debug)]
pub struct MigrationResult {
    pub entity: String,
    pub table: String,
    pub format_before: String,
    pub size_before: u64,
    pub entries: usize,
    pub unique_hashes: usize,
    pub collisions: usize,
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
                "Tabla: {}/{}\n  Ya en v2, nada que hacer.\n",
                self.entity, self.table
            );
            return;
        }
        if let Some(ref err) = self.error {
            println!(
                "Tabla: {}/{}\n  ERROR: {}\n",
                self.entity, self.table, err
            );
            return;
        }

        let reduction = if self.size_before > 0 {
            ((self.size_before as f64 - self.size_after as f64) / self.size_before as f64 * 100.0) as i64
        } else {
            0
        };

        println!("Tabla: {}/{}", self.entity, self.table);
        println!("  Formato antes:        {}", self.format_before);
        println!("  Tamaño antes:         {} bytes", self.size_before);
        println!("  Entradas:             {}", self.entries);
        println!("  Colisiones detectadas: {}", self.collisions);
        println!("  Formato después:      v2");
        println!("  Tamaño después:       {} bytes", self.size_after);
        println!("  Reducción:            {}%", reduction);
        println!(
            "  Backup:               {}",
            self.backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "no".to_string())
        );
        println!("  Tiempo:               {} ms", self.elapsed_ms);
        println!();

        if self.collisions > 0 {
            warn!(
                "{}: {} colisiones u128 detectadas durante migración. \
                 Esto indica que el dataset tenía PKs que ahora son equivalentes bajo NFC. \
                 Las filas con PKs canónicamente iguales colapsan en la misma entrada de índice. \
                 Las filas originales siguen en los segmentos pero solo una es alcanzable por PK. \
                 Revisión manual recomendada.",
                format!("{}/{}", self.entity, self.table),
                self.collisions
            );
        }
    }
}

/// Detect the format of a primary.idx file.
/// Returns: (version, is_legacy)
fn detect_format(path: &Path) -> Result<(u8, bool)> {
    let mut file = BufReader::new(fs::File::open(path)?);
    let mut header = [0u8; 8];
    let n = file.read(&mut header)?;

    if n >= 4 && header[..4] == PRIMARY_IDX_MAGIC {
        Ok((header[4], false))
    } else {
        // No magic → legacy (treat as v1 without header)
        Ok((1, true))
    }
}

/// Migrate a single table's primary.idx to v2.
pub fn migrate_table(
    entity: &str,
    table: &str,
    table_dir: &Path,
    dry_run: bool,
    keep_backup: bool,
    force: bool,
) -> MigrationResult {
    let start = Instant::now();
    let idx_path = table_dir.join("primary.idx");

    let mut result = MigrationResult {
        entity: entity.to_string(),
        table: table.to_string(),
        format_before: String::new(),
        size_before: 0,
        entries: 0,
        unique_hashes: 0,
        collisions: 0,
        size_after: 0,
        backup_path: None,
        elapsed_ms: 0,
        skipped: false,
        error: None,
    };

    if !idx_path.exists() {
        result.error = Some("primary.idx no encontrado".to_string());
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    let metadata = match fs::metadata(&idx_path) {
        Ok(m) => m,
        Err(e) => {
            result.error = Some(format!("no se pudo leer metadata: {}", e));
            result.elapsed_ms = start.elapsed().as_millis();
            return result;
        }
    };
    result.size_before = metadata.len();

    let (version, is_legacy) = match detect_format(&idx_path) {
        Ok(v) => v,
        Err(e) => {
            result.error = Some(format!("no se pudo detectar formato: {}", e));
            result.elapsed_ms = start.elapsed().as_millis();
            return result;
        }
    };

    result.format_before = if is_legacy {
        "v1 (legacy, sin header)".to_string()
    } else {
        format!("v{}", version)
    };

    if version >= 2 && !force {
        result.skipped = true;
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    // For v1/legacy: read as HashMap<String, _>, hash each key.
    // For v2 with --force: read as HashMap<u128, _>, re-serialize (no re-hashing needed).
    if version >= 2 && force {
        // v2 force-rewrite: read existing v2 index and re-save it.
        match primary_index_io::load_primary_index(&idx_path) {
            Ok(idx) => {
                result.entries = idx.len();
                result.unique_hashes = idx.len();

                if !dry_run {
                    let tmp_path = idx_path.with_extension("idx.tmp");
                    if let Err(e) = primary_index_io::save_primary_index(&tmp_path, &idx) {
                        result.error = Some(format!("error reescribiendo v2: {}", e));
                        result.elapsed_ms = start.elapsed().as_millis();
                        return result;
                    }
                    let _ = fs::rename(&tmp_path, &idx_path);
                    result.size_after = fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);
                }
            }
            Err(e) => {
                result.error = Some(format!("error leyendo v2 existente: {}", e));
                result.elapsed_ms = start.elapsed().as_millis();
                return result;
            }
        }
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    // Read the legacy/v1 payload.
    let legacy_map: HashMap<String, (u64, u32)> = {
        let file = match fs::File::open(&idx_path) {
            Ok(f) => f,
            Err(e) => {
                result.error = Some(format!("no se pudo abrir: {}", e));
                result.elapsed_ms = start.elapsed().as_millis();
                return result;
            }
        };
        let mut reader = BufReader::new(file);

        // Skip header if present.
        if !is_legacy {
            let mut header = [0u8; 8];
            let _ = reader.read(&mut header);
        }

        match bincode::deserialize_from(reader) {
            Ok(m) => m,
            Err(e) => {
                result.error = Some(format!("no se pudo deserializar: {}", e));
                result.elapsed_ms = start.elapsed().as_millis();
                return result;
            }
        }
    };

    result.entries = legacy_map.len();

    // Build v2 index with hashing.
    let mut new_index = PrimaryIndex::with_capacity(legacy_map.len());
    let mut collisions = 0usize;

    for (pk, loc) in legacy_map {
        let h = xxh3_128(&canonical_bytes(&pk));
        if new_index.insert_raw(h, loc).is_some() {
            collisions += 1;
        }
    }

    result.unique_hashes = new_index.len();
    result.collisions = collisions;

    if dry_run {
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    // Write v2.
    let backup_path = idx_path.with_extension("idx.v1.bak");
    let tmp_path = idx_path.with_extension("idx.tmp");

    // Backup original.
    if let Err(e) = fs::rename(&idx_path, &backup_path) {
        result.error = Some(format!("no se pudo crear backup: {}", e));
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    // Write new v2 file.
    if let Err(e) = (|| -> Result<()> {
        let mut file = std::io::BufWriter::new(fs::File::create(&tmp_path)?);
        file.write_all(&PRIMARY_IDX_MAGIC)?;
        file.write_all(&[PRIMARY_IDX_VERSION_V2, 0, 0, 0])?;
        bincode::serialize_into(&mut file, new_index.inner())?;
        file.flush()?;
        file.into_inner()
            .context("BufWriter into_inner")?
            .sync_all()?;
        fs::rename(&tmp_path, &idx_path)?;
        Ok(())
    })() {
        // Rollback: restore backup.
        let _ = fs::rename(&backup_path, &idx_path);
        result.error = Some(format!("error escribiendo v2: {}", e));
        result.elapsed_ms = start.elapsed().as_millis();
        return result;
    }

    // Get new size.
    result.size_after = fs::metadata(&idx_path).map(|m| m.len()).unwrap_or(0);

    if keep_backup {
        result.backup_path = Some(backup_path);
    } else {
        let _ = fs::remove_file(&backup_path);
    }

    info!(
        "Migrado {}/{}: {} entradas, {} colisiones, {}ms",
        entity, table, result.entries, result.collisions, start.elapsed().as_millis()
    );

    result.elapsed_ms = start.elapsed().as_millis();
    result
}

/// Find all table directories containing a primary.idx file.
pub fn find_all_tables(data_root: &Path) -> Vec<(String, String, PathBuf)> {
    let mut tables = Vec::new();
    let mirror_dir = data_root.join("mirror");

    let entity_dirs: Vec<PathBuf> = if mirror_dir.exists() {
        // New layout: mirror/<entity>/<table>/
        fs::read_dir(&mirror_dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect()
    } else {
        // Legacy layout: data_root/<entity>/<table>/
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
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let table_dir = entry.path();
                    if table_dir.join("primary.idx").exists() {
                        let table_name = entry
                            .file_name()
                            .to_string_lossy()
                            .into_owned();
                        tables.push((entity_name.clone(), table_name, table_dir));
                    }
                }
            }
        }
    }

    tables
}

/// Run migration for all tables.
pub fn migrate_all(
    data_root: &Path,
    dry_run: bool,
    keep_backup: bool,
    force: bool,
) -> Vec<MigrationResult> {
    let tables = find_all_tables(data_root);
    let mut results = Vec::with_capacity(tables.len());

    for (entity, table, table_dir) in tables {
        let result = migrate_table(&entity, &table, &table_dir, dry_run, keep_backup, force);
        result.print_report();
        results.push(result);
    }

    // Print aggregate summary.
    let total = results.len();
    let skipped = results.iter().filter(|r| r.skipped).count();
    let migrated = results.iter().filter(|r| !r.skipped && r.error.is_none()).count();
    let with_collisions = results.iter().filter(|r| r.collisions > 0).count();
    let errors = results.iter().filter(|r| r.error.is_some()).count();
    let total_ms: u128 = results.iter().map(|r| r.elapsed_ms).sum();

    println!("=== Migración completada ===");
    println!("Tablas inspeccionadas:    {}", total);
    println!("Ya en v2:                 {}", skipped);
    println!("Migradas:                 {}", migrated);
    println!("Con colisiones:           {}", with_collisions);
    println!("Errores:                  {}", errors);
    println!("Tiempo total:             {} ms", total_ms);

    results
}
