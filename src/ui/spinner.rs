use anyhow::{Result, anyhow};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle, ProgressState};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crossterm::{cursor, execute, terminal};
use crossterm::event::{self, Event, KeyCode};

/// Executes a long-running task with a "Gemini Status Toast" spinner.
///
/// Handles:
/// - Cursor positioning at `start_y`.
/// - Indentation using `indent`.
/// - Spinner creation and styling.
/// - Cancellation via ESC key (background thread).
/// - Cleanup on finish (success or error).
///
/// Arguments:
/// - `initial_msg`: Message to display initially.
/// - `start_y`: Y-coordinate to render the spinner.
/// - `indent`: Number of spaces to indent the message (X-offset).
/// - `task`: A closure that takes `&ProgressBar` (to update message) and `&Arc<AtomicBool>` (to check cancellation).
pub fn run_with_spinner<F, T>(
    initial_msg: &str,
    start_y: u16,
    indent: u16,
    task: F,
) -> Result<T>
where
    F: FnOnce(&ProgressBar, &Arc<AtomicBool>) -> Result<T>,
{
    // Flag de cancelación compartido
    let should_cancel = Arc::new(AtomicBool::new(false));
    let should_cancel_clone = should_cancel.clone();

    // Hilo para detectar ESC
    thread::spawn(move || {
        loop {
            if should_cancel_clone.load(Ordering::Relaxed) {
                break;
            }
            if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                if let Ok(Event::Key(key)) = event::read() {
                    if key.code == KeyCode::Esc {
                        should_cancel_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }
    });

    // Posicionar cursor y limpiar línea
    let _ = execute!(
        std::io::stdout(),
        cursor::MoveTo(0, start_y),
        terminal::Clear(terminal::ClearType::UntilNewLine)
    );

            // Configuración visual (Estilo Gemini)

            let padding = " ".repeat(indent as usize);

            let lila_custom = "\x1b[38;2;197;137;249m";

            let gray_opaque = "\x1b[38;2;150;150;150m";

            let reset = "\x1b[0m";
    
            let template_str = format!(
                "{}{}{{spinner}} {{msg}}{} {}(esc to cancel, {{human_elapsed}}){}",
                padding, lila_custom, reset, gray_opaque, reset
            );

            let spinner_style = ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠁⠈ ")
                .template(&template_str)?
                .with_key("human_elapsed", |state: &ProgressState, w: &mut dyn std::fmt::Write| {
                    let secs = state.elapsed().as_secs();
                    if secs < 60 {
                        write!(w, "{}s", secs).unwrap();
                    } else if secs < 3600 {
                        write!(w, "{}min {}s", secs / 60, secs % 60).unwrap();
                    } else {
                        write!(w, "{}h {}min", secs / 3600, (secs % 3600) / 60).unwrap();
                    }
                });

    // Forzar uso de stdout
    let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::stdout());
    spinner.set_style(spinner_style);
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner.set_message(initial_msg.to_string());

    // Ejecutar tarea
    let result = task(&spinner, &should_cancel);

    // Cleanup
    // Detener hilo de input
    should_cancel.store(true, Ordering::Relaxed);
    
    // Limpiar spinner
    spinner.finish_and_clear();
    let _ = std::io::stdout().write_all(b"\r\x1b[2K"); // Limpieza ANSI explícita
    let _ = std::io::stdout().flush();

    match result {
        Ok(val) => {
            // Verificar si fue cancelado justo antes de terminar
             if should_cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("Operation cancelled by user"));
            }
            Ok(val)
        },
        Err(e) => {
             if should_cancel.load(Ordering::Relaxed) {
                return Err(anyhow!("Operation cancelled by user"));
            }
            Err(e)
        }
    }
}
