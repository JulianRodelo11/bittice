use anyhow::{Result, anyhow};
use indicatif::ProgressBar;
use roaring::RoaringBitmap;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::core::config::{Config, FieldConfig, FieldMetadata, FieldStats};
use crate::core::date_utils::{extract_day, extract_hour_bucket, extract_month};

struct FieldWriters {
    dat: File,
    offsets: File,
    bitmap_file: File,
    meta_file: File,
    value_bitmaps: HashMap<String, RoaringBitmap>,
    count: u64,
    current_offset: u64,
}

pub fn process_and_write(
    input_path: &str,
    output_dir: &Path,
    detected_fields: &HashMap<String, FieldStats>,
    _pb: &ProgressBar,
    cancel_flag: &AtomicBool,
) -> Result<()> {
    // Preparar directorios
    fs::create_dir_all(output_dir.join("index"))?;
    fs::create_dir_all(output_dir.join("stores"))?;

    // 1. Definir campos a generar (Expansión de fechas)
    let mut fields_to_process: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_target_fields: HashSet<String> = HashSet::new();

    for (name, stats) in detected_fields {
        let mut subfields = vec![name.clone()];
        if stats.is_date {
            subfields.push(format!("{}_date", name));
            subfields.push(format!("{}_day", name));
            subfields.push(format!("{}_month", name));
            if stats.has_time {
                subfields.push(format!("{}_hour_bucket", name));
            }
        }
        for sf in &subfields {
            all_target_fields.insert(sf.clone());
        }
        fields_to_process.insert(name.clone(), subfields);
    }

    // 2. Guardar config.json
    let mut indexed_fields = Vec::new();
    let mut columnar_fields = Vec::new();
    for (name, stats) in detected_fields {
        indexed_fields.push(FieldConfig {
            field_name: name.clone(),
            indexed: true,
            columnar: true,
            extract_date_day: stats.is_date,
        });
        columnar_fields.push(name.clone());
    }
    let config = Config {
        indexed_fields,
        columnar_fields,
    };
    let config_file = File::create(output_dir.join("config.json"))?;
    serde_json::to_writer_pretty(config_file, &config)?;

    // 3. Inicializar Writers
    let mut writers: HashMap<String, FieldWriters> = HashMap::new();
    for field_name in &all_target_fields {
        writers.insert(field_name.clone(), create_writers(output_dir, field_name)?);

        // Archivos extra de Bittice
        File::create(output_dir.join(format!("stores/{}_threads.dat", field_name)))?;
        File::create(output_dir.join(format!("stores/{}_metadata.dat", field_name)))?;
        File::create(
            output_dir.join(format!("stores/{}_columnar_{}.dat", field_name, field_name)),
        )?;
    }

    // 4. Leer y Escribir (Pasada 2)
    let file = File::open(input_path)?;
    let reader = BufReader::new(file);

    let mut records_processed = 0u32;

    for line in reader.lines() {
        if cancel_flag.load(Ordering::Relaxed) {
             return Err(anyhow!("Operation cancelled by user"));
        }

        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let internal_id = records_processed;

        for (base_field, derived_names) in &fields_to_process {
            let val_raw = v.get(base_field);
            
            // Si el campo no existe, escribimos una entrada vacía para mantener el alineamiento de IDs
            let val_str = match val_raw {
                Some(Value::String(s)) => std::borrow::Cow::Borrowed(s.as_str()),
                Some(Value::Number(n)) => std::borrow::Cow::Owned(n.to_string()),
                Some(Value::Bool(b)) => std::borrow::Cow::Owned(b.to_string()),
                Some(Value::Null) => std::borrow::Cow::Borrowed(""),
                Some(o) => std::borrow::Cow::Owned(o.to_string()),
                None => std::borrow::Cow::Borrowed(""),
            };

            for target_name in derived_names {
                let final_value: String = if target_name == base_field {
                    val_str.to_string()
                } else if target_name.ends_with("_date") || target_name.ends_with("_day") {
                    extract_day(&val_str).unwrap_or_default()
                } else if target_name.ends_with("_month") {
                    extract_month(&val_str).unwrap_or_default()
                } else if target_name.ends_with("_hour_bucket") {
                    extract_hour_bucket(&val_str).unwrap_or_default()
                } else {
                    String::new()
                };

                if let Some(writer) = writers.get_mut(target_name) {
                    write_record(writer, target_name, internal_id, &final_value)?;
                }
            }
        }
        records_processed += 1;
    }

    // 5. Cerrar y serializar metadatos finales
    for (name, mut w) in writers {
        // Serializar el HashMap de bitmaps por valor
        let encoded = bincode::serialize(&w.value_bitmaps)?;
        w.bitmap_file.write_all(&encoded)?;

        let meta = FieldMetadata {
            name: name.clone(),
            count: w.count,
        };
        let meta_bin = bincode::serialize(&meta)?;
        w.meta_file.write_all(&meta_bin)?;
    }

    Ok(())
}

fn create_writers(base: &Path, field: &str) -> Result<FieldWriters> {
    Ok(FieldWriters {
        dat: File::create(base.join(format!("stores/{}.dat", field)))?,
        offsets: File::create(base.join(format!("stores/{}.offsets", field)))?,
        bitmap_file: File::create(base.join(format!("index/bitmaps_{}.dat", field)))?,
        meta_file: File::create(base.join(format!("index/metadata_{}.dat", field)))?,
        value_bitmaps: HashMap::new(),
        count: 0,
        current_offset: 0,
    })
}

fn write_record(w: &mut FieldWriters, _field_name: &str, id: u32, val: &str) -> Result<()> {
    // Guardar offset actual
    w.offsets.write_all(&w.current_offset.to_le_bytes())?;

    // Guardar dato binario
    let binary_val = bincode::serialize(val)?;
    let len = binary_val.len() as u64;
    w.dat.write_all(&binary_val)?;
    
    w.current_offset += len;

    if !val.is_empty() {
        w.value_bitmaps.entry(val.to_string()).or_default().insert(id);
    }
    w.count += 1;
    Ok(())
}
