use std::sync::Arc;

use tracing_subscriber::{EnvFilter, fmt, fmt::writer::BoxMakeWriter};

/// Log file used when running the interactive TUI, so log lines don't corrupt
/// the alternate-screen display.
const TUI_LOG_FILE: &str = "hiroz-console.log";

pub fn init_logger(json_mode: bool, debug: bool, tui: bool) {
    // Build filter from RUST_LOG environment variable or default
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if debug {
            EnvFilter::new("hiroz=debug,zenoh=debug")
        } else {
            EnvFilter::new("hiroz=info,zenoh=warn")
        }
    });

    // In TUI mode, stderr is the terminal that ratatui is drawing to, so any log
    // line written there bleeds over the UI. Redirect logs to a file instead.
    // In headless/export mode, stderr is fine (and useful for real-time output).
    let writer = if tui {
        match std::fs::File::create(TUI_LOG_FILE) {
            Ok(file) => {
                eprintln!("hiroz-console: logging to {TUI_LOG_FILE}");
                BoxMakeWriter::new(Arc::new(file))
            }
            Err(e) => {
                eprintln!(
                    "hiroz-console: could not open {TUI_LOG_FILE} ({e}); logging to stderr"
                );
                BoxMakeWriter::new(std::io::stderr)
            }
        }
    } else {
        BoxMakeWriter::new(std::io::stderr)
    };

    // Logs going to a file must never emit ANSI escape codes.
    let ansi = !tui;

    if json_mode {
        // Structured JSON logs
        fmt()
            .json()
            .with_target(true)
            .with_current_span(false)
            .with_writer(writer)
            .with_ansi(false)
            .with_env_filter(filter)
            .init();
    } else {
        // Human-readable logs
        fmt()
            .compact()
            .with_writer(writer)
            .with_ansi(ansi)
            .with_env_filter(filter)
            .init();
    }
}
