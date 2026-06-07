//! `tracing` setup with rolling daily file appender under
//! `~/.cadenza/logs/` (7-file retention) plus a stderr layer.
//!
//! Per DESIGN-desktop-v2.md § "Observabilidade":
//! - Log lines are always English (no i18n on logs).
//! - `CADENZA_LOG` env overrides the level (default `info`).
//! - Token redaction is the responsibility of `auth.rs`; this module
//!   only configures the subscriber.

use std::backtrace::Backtrace;
use std::path::PathBuf;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

/// Initialize the global subscriber. The returned `WorkerGuard` must be
/// kept alive until process exit — dropping it flushes the non-blocking
/// writer, so a premature drop loses tail lines.
pub fn init() -> Result<WorkerGuard, std::io::Error> {
    let log_dir = log_dir();
    std::fs::create_dir_all(&log_dir)?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("cadenza")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&log_dir)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("CADENZA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = fmt::layer().with_writer(non_blocking).with_ansi(false);
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// Install a global panic hook that logs the panic (message, location,
/// and a captured backtrace) through the `tracing` subscriber BEFORE the
/// process unwinds, so crashes land in the rolling log file. The
/// previously installed hook is chained and invoked afterwards, so the
/// default panic behavior (stderr message, abort/unwind) is preserved.
///
/// Call this once, right after the subscriber is initialized.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // `Backtrace::capture` honors `RUST_BACKTRACE` / `RUST_LIB_BACKTRACE`;
        // it returns a disabled backtrace when neither is set.
        let backtrace = Backtrace::capture();
        let message = panic_message(info.payload());
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown location>".to_string());
        tracing::error!("{}", format_panic(&location, message, &backtrace));
        previous(info);
    }));
}

/// Extract a human-readable message from a panic payload, which is either
/// a `&str` (from `panic!("...")`) or a `String` (from a formatted panic).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string panic payload>"
    }
}

/// Format a panic into a single English log line carrying the source
/// location, payload message, and captured backtrace. Extracted from the
/// hook so the formatting is unit-testable without triggering an actual
/// panic.
fn format_panic(location: &str, message: &str, backtrace: &Backtrace) -> String {
    format!("panic at {location}: {message}\nbacktrace:\n{backtrace}")
}

/// `~/.cadenza/logs/` — falls back to the system temp dir if there's
/// no home directory, so we never panic during early boot.
pub fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_message_extracts_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom");
        assert_eq!(panic_message(payload.as_ref()), "boom");
    }

    #[test]
    fn panic_message_extracts_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("kaboom 42"));
        assert_eq!(panic_message(payload.as_ref()), "kaboom 42");
    }

    #[test]
    fn panic_message_handles_unknown_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(7u32);
        assert_eq!(
            panic_message(payload.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn format_panic_includes_location_message_and_backtrace() {
        let backtrace = Backtrace::capture();
        let line = format_panic("src/foo.rs:12:5", "everything broke", &backtrace);
        assert!(line.contains("panic at src/foo.rs:12:5: everything broke"));
        assert!(line.contains("backtrace:"));
    }
}
