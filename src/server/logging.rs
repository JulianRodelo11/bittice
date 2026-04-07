use tracing_subscriber::fmt::format::{FormatEvent, FormatFields};
use tracing_subscriber::fmt::{format, FmtContext};
use tracing_subscriber::registry::LookupSpan;
use tracing::{Level, Event, Subscriber};
use std::fmt;
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
            "\x1b[33m⚠\x1b[0m"
        } else if *level == Level::DEBUG || *level == Level::TRACE {
            "\x1b[90m◇\x1b[0m"
        } else {
            "\x1b[32m◆\x1b[0m"
        };

        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        let msg = visitor.message;

        let term = Term::stdout();
        let width = term.size_checked().map(|(_, w)| w as usize).unwrap_or(100);
        let indent_width = 4;
        let max_text_width = if width > indent_width + 10 { width - indent_width - 5 } else { 40 };

        let wrapped = wrap_text(&msg, max_text_width);
        for (i, line) in wrapped.into_iter().enumerate() {
            if i == 0 {
                write!(writer, "\x1b[34m│\x1b[0m  {} {}", symbol, line)?;
            } else {
                write!(writer, "\n\x1b[34m│\x1b[0m     {}", line)?;
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

pub fn init_logging() {
    let _ = tracing_subscriber::fmt()
        .event_format(CliclackFormatter)
        .try_init();
}
