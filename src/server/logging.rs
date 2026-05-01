use tracing_subscriber::fmt::format::{FormatEvent, FormatFields};
use tracing_subscriber::fmt::{format, FmtContext};
use tracing_subscriber::registry::LookupSpan;
use tracing::{Level, Event, Subscriber};
use std::fmt;
use std::io::LineWriter;
use console::Term;

pub struct CliclackFormatter;

impl<S, N> FormatEvent<S, N> for CliclackFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        _ctx: &FmtContext<'_, S, N>,
        mut writer: format::Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let metadata = event.metadata();
        let level = metadata.level();
        
        let symbol = if *level == Level::ERROR {
            "\x1b[31m▲\x1b[0m"
        } else if *level == Level::WARN {
            "\x1b[33m▲\x1b[0m" // Yellow solid triangle for warnings
        } else if *level == Level::DEBUG || *level == Level::TRACE {
            "\x1b[90m◇\x1b[0m"
        } else {
            "\x1b[32m◆\x1b[0m" // INFO: Green solid diamond to match cliclack prompts
        };

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let msg = visitor.message;

        let width: usize = if std::path::Path::new("/.dockerenv").exists() {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|&w| w > 20)
                .unwrap_or(120)
        } else {
            let term = Term::stdout();
            term.size_checked().map(|(_, w)| w as usize).unwrap_or(120)
        };
        let max_text_width = if width > 20 { width.saturating_sub(10) } else { 100 };

        let wrapped = wrap_text(&msg, max_text_width);
        for (i, line) in wrapped.into_iter().enumerate() {
            if i == 0 {
                write!(writer, "{}  {}", symbol, line)?;
            } else {
                write!(writer, "\n\x1b[34m│\x1b[0m  {}", line)?;
            }
        }
        writeln!(writer)
    }
}

#[derive(Default)]
struct MessageVisitor {
    message: String,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{:?}", value);
            if self.message.starts_with('"') && self.message.ends_with('"') {
                self.message = self.message[1..self.message.len()-1].to_string();
            }
        }
    }
}

fn wrap_text(text: &str, limit: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current_line = String::new();
        for word in paragraph.split_whitespace() {
            if current_line.is_empty() {
                current_line.push_str(word);
            } else if current_line.len() + 1 + word.len() <= limit {
                current_line.push(' ');
                current_line.push_str(word);
            } else {
                lines.push(current_line);
                current_line = word.to_string();
            }
        }
        if !current_line.is_empty() {
            lines.push(current_line);
        }
    }
    if lines.is_empty() { lines.push(String::new()); }
    lines
}

struct MultiWriter<W1: std::io::Write, W2: std::io::Write> {
    w1: W1,
    w2: W2,
}

impl<W1: std::io::Write, W2: std::io::Write> std::io::Write for MultiWriter<W1, W2> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.w1.write(buf)?;
        self.w2.write_all(&buf[..n])?;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.w1.flush()?;
        self.w2.flush()?;
        Ok(())
    }
}

struct DiscardStdout;

impl std::io::Write for DiscardStdout {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// When `quiet_stdout` is true (interactive wizard / `setup`), tracing still mirrors to
/// the engine log file under the data root (e.g. `data/server.log`) but skips the real stdout so engine noise does not break cliclack.
pub fn init_logging(quiet_stdout: bool) {
    let root = crate::core::data_paths::resolved_data_root();
    let _ = std::fs::create_dir_all(&root);
    let _ = crate::core::data_paths::migrate_legacy_layout();
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::core::data_paths::server_log_path());

    let subscriber = tracing_subscriber::fmt()
        .event_format(CliclackFormatter);

    if let Ok(file) = log_file {
        if quiet_stdout {
            let file_for_writer = file.try_clone().expect("Failed to clone log file");
            let _ = subscriber
                .with_writer(move || {
                    let f = file_for_writer
                        .try_clone()
                        .expect("Failed to clone log file for writer");
                    MultiWriter {
                        w1: DiscardStdout,
                        w2: f,
                    }
                })
                .try_init();
        } else {
            let file_for_writer = file.try_clone().expect("Failed to clone log file");
            let _ = subscriber
                .with_writer(move || {
                    let stdout = LineWriter::new(std::io::stdout());
                    let f = file_for_writer
                        .try_clone()
                        .expect("Failed to clone log file for writer");
                    MultiWriter { w1: stdout, w2: f }
                })
                .try_init();
        }
    } else {
        let _ = subscriber.try_init();
    }
}
