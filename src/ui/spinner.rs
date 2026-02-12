use anyhow::Result;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crossterm::{cursor, execute, terminal};
use crossterm::event::{self, Event, KeyCode};

pub fn run_with_spinner<F, T>(
    initial_msg: &str,
    _start_y: u16,
    indent: u16,
    task: F,
) -> Result<T>
where
    F: FnOnce(&ProgressBar, &Arc<AtomicBool>) -> Result<T>,
{
    let should_cancel = Arc::new(AtomicBool::new(false));
    let should_cancel_clone = should_cancel.clone();
    let is_finished = Arc::new(AtomicBool::new(false));
    let is_finished_clone = is_finished.clone();

    // Hilo para detectar ESC
    thread::spawn(move || {
        loop {
            if is_finished_clone.load(Ordering::Relaxed) || should_cancel_clone.load(Ordering::Relaxed) {
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

    // Obtener la última línea de la terminal para el spinner
    let (_, height) = terminal::size().unwrap_or((80, 24));
    let target_y = height.saturating_sub(1);

    // Mover cursor al final y limpiar antes de empezar
    let _ = execute!(
        std::io::stdout(),
        cursor::MoveTo(0, target_y),
        terminal::Clear(terminal::ClearType::CurrentLine)
    );

    let padding = " ".repeat(indent as usize);
    let lila_custom = "\x1b[38;2;197;137;249m";
    let reset = "\x1b[0m";

    let template_str = format!("{}{}{{spinner}} {{msg}}{}", padding, lila_custom, reset);

    let spinner_style = ProgressStyle::default_spinner()
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠁⠈ ")
        .template(&template_str)?;

    let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::stdout());
    spinner.set_style(spinner_style);
    spinner.enable_steady_tick(Duration::from_millis(80));
    spinner.set_message(initial_msg.to_string());

    // Ejecutar tarea
    let result = task(&spinner, &should_cancel);

    // Cleanup: Detener hilo, limpiar spinner y borrar línea físicamente
    is_finished.store(true, Ordering::Relaxed);
    spinner.finish_and_clear(); 

    let _ = execute!(
        std::io::stdout(),
        cursor::MoveTo(0, target_y),
        terminal::Clear(terminal::ClearType::CurrentLine)
    );
    let _ = std::io::stdout().flush();

    result
}
