use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use serde_json::Value;
use anyhow::{Result, Context, anyhow};
use crate::core::config::FieldStats;
use crate::core::date_utils::{is_date_format, has_time_component};
use indicatif::ProgressBar;

pub fn analyze_schema(input_path: &str, _pb: &ProgressBar, cancel_flag: &AtomicBool) -> Result<HashMap<String, FieldStats>> {
    let file = File::open(input_path).context("Could not open file for analysis")?;
    let reader = BufReader::new(file);
    
    let mut detected_fields: HashMap<String, FieldStats> = HashMap::new();

    for line in reader.lines() {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(anyhow!("Operation cancelled by user"));
        }

        let line = line?;
        if line.trim().is_empty() { continue; }
        
        // El spinner ya tiene steady_tick configurado en load.rs, 
        // no necesitamos llamar a pb.tick() manualmente aquí.
        
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
