use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use serde_json::Value;
use anyhow::{Result, Context};
use crate::core::config::FieldStats;
use crate::core::date_utils::{is_date_format, has_time_component};
use indicatif::ProgressBar;

pub fn analyze_schema(input_path: &str, pb: &ProgressBar) -> Result<HashMap<String, FieldStats>> {
    let file = File::open(input_path).context("No se pudo abrir el archivo para análisis")?;
    let reader = BufReader::new(file);
    
    let mut detected_fields: HashMap<String, FieldStats> = HashMap::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() { continue; }
        
        // Actualizar barra de progreso (spinner)
        pb.tick();
        
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(obj) = v.as_object() {
                for (key, val) in obj {
                    let s_val = val.as_str().unwrap_or("");
                    let is_date = is_date_format(s_val);
                    let has_time = is_date && has_time_component(s_val);

                    detected_fields.entry(key.clone())
                        .and_modify(|stats| {
                            stats.is_date = stats.is_date || is_date;
                            stats.has_time = stats.has_time || has_time;
                        })
                        .or_insert(FieldStats { is_date, has_time });
                }
            }
        }
    }

    Ok(detected_fields)
}
