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
    
    struct FieldAnalysis {
        total_non_empty: usize,
        date_matches: usize,
        time_matches: usize,
    }
    let mut analysis: HashMap<String, FieldAnalysis> = HashMap::new();

    for line in reader.lines() {
        if cancel_flag.load(Ordering::Relaxed) {
            return Err(anyhow!("Operation cancelled by user"));
        }

        let line = line?;
        if line.trim().is_empty() { continue; }
        
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(obj) = v.as_object() {
                for (key, val) in obj {
                    let s_val = val.as_str().unwrap_or("");
                    if s_val.is_empty() { continue; }

                    let entry = analysis.entry(key.clone()).or_insert(FieldAnalysis {
                        total_non_empty: 0,
                        date_matches: 0,
                        time_matches: 0,
                    });

                    entry.total_non_empty += 1;
                    if is_date_format(s_val) {
                        entry.date_matches += 1;
                        if has_time_component(s_val) {
                            entry.time_matches += 1;
                        }
                    }
                }
            }
        }
    }

    let mut detected_fields = HashMap::new();
    for (key, stats) in analysis {
        let key_upper = key.to_uppercase();
        // Estrategia Lattice: Prioridad si el nombre contiene DATE o DATETIME
        let name_suggests_date = key_upper.contains("DATE") || key_upper.contains("DATETIME");
        
        // Si el nombre lo sugiere, somos más flexibles. Si no, pedimos > 50% de coincidencia.
        let is_date = name_suggests_date || (
            stats.total_non_empty > 0 && 
            (stats.date_matches as f32 / stats.total_non_empty as f32) > 0.5
        );
        
        let has_time = is_date && (
            (stats.time_matches as f32 / stats.total_non_empty as f32) > 0.1
        );

        detected_fields.insert(key, FieldStats { is_date, has_time });
    }

    Ok(detected_fields)
}
