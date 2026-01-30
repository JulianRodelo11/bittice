use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path};
use std::collections::{HashMap, HashSet};
use serde_json::Value;
use roaring::RoaringBitmap;
use anyhow::{Result};
use indicatif::ProgressBar;

use crate::core::config::{Config, FieldConfig, FieldMetadata, FieldStats};
use crate::core::date_utils::{extract_day, extract_month, extract_hour_bucket};

struct FieldWriters {
    idx: File,
    store: File,
    dat: File,
    bitmap_file: File,
    meta_file: File,
    bitmap: RoaringBitmap,
    count: u64,
}

pub fn process_and_write(
    input_path: &str, 
    output_dir: &Path, 
    detected_fields: &HashMap<String, FieldStats>,
    pb: &ProgressBar
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
            subfields.push(format!("{}_DATE", name));
            subfields.push(format!("{}_DAY", name));
            subfields.push(format!("{}_MONTH", name));
            if stats.has_time {
                subfields.push(format!("{}_HOUR_BUCKET", name));
            }
        }
        for sf in &subfields { all_target_fields.insert(sf.clone()); }
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
    let config = Config { indexed_fields, columnar_fields };
    let config_file = File::create(output_dir.join("config.json"))?;
    serde_json::to_writer_pretty(config_file, &config)?;

    // 3. Inicializar Writers
    let mut writers: HashMap<String, FieldWriters> = HashMap::new();
    for field_name in &all_target_fields {
        writers.insert(field_name.clone(), create_writers(output_dir, field_name)?);
        
        // Archivos extra de Bittice
        File::create(output_dir.join(format!("stores/{}_threads.dat", field_name)))?;
        File::create(output_dir.join(format!("stores/{}_metadata.dat", field_name)))?;
        File::create(output_dir.join(format!("stores/{}_columnar_{}.dat", field_name, field_name)))?;
    }

    // 4. Leer y Escribir (Pasada 2)
    let file = File::open(input_path)?;
    let reader = BufReader::new(file);

    for (i, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() { continue; } 
        
        // Actualizar barra de progreso
        if i % 1000 == 0 { pb.inc(1000); }

        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let internal_id = i as u32;

        for (base_field, derived_names) in &fields_to_process {
            let val_raw = v.get(base_field);
            if val_raw.is_none() { continue; } 
            
            // CORRECCIÓN: Manejo correcto de Value::String
            let val_str = match val_raw.unwrap() {
                Value::String(s) => s.as_str(), 
                Value::Null => "",
                _ => "0", 
            };
            if val_str.is_empty() { continue; } 

            for target_name in derived_names {
                // CORRECCIÓN: Tipado explícito para evitar confusión del compilador
                let final_value: Option<String> = if target_name == base_field {
                    Some(val_str.to_string())
                } else if target_name.ends_with("_date") || target_name.ends_with("_day") {
                    extract_day(val_str)
                } else if target_name.ends_with("_month") {
                    extract_month(val_str)
                } else if target_name.ends_with("_hour_bucket") {
                    extract_hour_bucket(val_str)
                } else {
                    None
                };

                if let Some(val) = final_value {
                    if let Some(writer) = writers.get_mut(target_name) {
                        write_record(writer, target_name, internal_id, &val)?;
                    }
                }
            }
        }
    }

    // 5. Cerrar y serializar metadatos finales
    for (name, mut w) in writers {
        w.bitmap.serialize_into(&mut w.bitmap_file)?;
        let meta = FieldMetadata { name: name.clone(), count: w.count };
        let meta_bin = bincode::serialize(&meta)?;
        w.meta_file.write_all(&meta_bin)?;
    }

    Ok(())
}

fn create_writers(base: &Path, field: &str) -> Result<FieldWriters> {
    Ok(FieldWriters {
        idx: File::create(base.join(format!("index/{}.idx", field)))?,
        store: File::create(base.join(format!("stores/{}.store", field)))?,
        dat: File::create(base.join(format!("stores/{}.dat", field)))?,
        bitmap_file: File::create(base.join(format!("index/bitmaps_{}.dat", field)))?,
        meta_file: File::create(base.join(format!("index/metadata_{}.dat", field)))?,
        bitmap: RoaringBitmap::new(),
        count: 0,
    })
}

fn write_record(w: &mut FieldWriters, field_name: &str, id: u32, val: &str) -> Result<()> {
    writeln!(w.idx, "{}__{}\t{}", field_name, val, id)?;
    writeln!(w.store, "{}\t{}", id, val)?;
    
    let binary_val = bincode::serialize(val)?;
    w.dat.write_all(&binary_val)?;
    
    w.bitmap.insert(id);
    w.count += 1;
    Ok(())
}
