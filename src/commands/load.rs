use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::atomic::Ordering;
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::fs::File;
use serde_json::Value;

use crate::core::schema::analyze_schema;
use crate::core::storage::table::Table;
use crate::core::date_utils::{extract_day, extract_hour_bucket, extract_month};
use crate::core::config::FieldStats;

/// Function to be called from the command line (without complex TUI)
pub fn execute_load_cli(input_path: &str, entity: &str, table: &str) -> Result<()> {
    let should_cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    
    // Simple console spinner
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(indicatif::ProgressStyle::default_spinner().template("{spinner:.green} {msg}")?);
    pb.enable_steady_tick(std::time::Duration::from_millis(100));

    pb.set_message("Pass 1: Analyzing schema...");
    let detected_fields = analyze_schema(input_path, &pb, &should_cancel)?;

    pb.set_message("Pass 2: Writing binary data and segments...");
    load_data_to_table(input_path, entity, table, &detected_fields, &should_cancel)?;

    pb.finish_with_message("✅ Data loaded successfully!");
    Ok(())
}

fn load_data_to_table(
    input_path: &str,
    entity: &str,
    table_name: &str,
    detected_fields: &HashMap<String, FieldStats>,
    should_cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    // 1. Clear existing data (Overwrite mode)
    let table_dir = Path::new("data").join(entity).join(table_name);
    if table_dir.exists() {
        std::fs::remove_dir_all(&table_dir)?;
    }

    // 2. Initialize Table
    let base_path = Path::new("data").join(entity);
    // Ensure entity directory exists
    if !base_path.exists() {
        std::fs::create_dir_all(&base_path)?;
    }
    let mut table = Table::open(&base_path, table_name)?;

    // Auto-detect Primary Key
    let candidates = ["pk", "id", "PK", "ID", "key", "KEY", "uuid", "UUID"];
    for cand in candidates {
        if detected_fields.contains_key(cand) {
            table.manifest.primary_key = cand.to_string();
            break;
        }
    }

    // Save original fields in the manifest
    let original_fields_list: Vec<String> = detected_fields.keys().cloned().collect();
    table.set_original_fields(original_fields_list)?;

    // 3. Prepare date expansion
    let mut fields_to_process: HashMap<String, Vec<String>> = HashMap::new();
    for (name, stats) in detected_fields {
        let mut subfields = vec![name.clone()];
        if stats.is_date {
            subfields.push(format!("{}_day", name));
            subfields.push(format!("{}_month", name));
            if stats.has_time {
                subfields.push(format!("{}_hour_bucket", name));
            }
        }
        fields_to_process.insert(name.clone(), subfields);
    }

    // 4. Read and Write
    let file = File::open(input_path)?;
    let reader = BufReader::new(file);

    for line in reader.lines() {
        if should_cancel.load(Ordering::Relaxed) {
             return Err(anyhow!("Operation cancelled by user"));
        }

        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let v: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
        let mut row_data: HashMap<String, String> = HashMap::new();

        // Process detected fields and expand dates
        for (base_field, derived_names) in &fields_to_process {
            let val_raw = v.get(base_field);
            
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
                } else if target_name.ends_with("_day") {
                    extract_day(&val_str).unwrap_or_default()
                } else if target_name.ends_with("_month") {
                    extract_month(&val_str).unwrap_or_default()
                } else if target_name.ends_with("_hour_bucket") {
                    extract_hour_bucket(&val_str).unwrap_or_default()
                } else {
                    String::new()
                };
                
                row_data.insert(target_name.clone(), final_value);
            }
        }
        
        // Insert into the new engine
        table.insert(row_data)?;
    }

    // 5. Final commit (Active flush)
    table.flush_active_segment()?;

    Ok(())
}
