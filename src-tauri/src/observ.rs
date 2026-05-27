//! `tracing` setup with rolling daily file appender under
//! `~/.cadenza/logs/` (7-file retention) plus a stderr layer.
//!
//! Per DESIGN-desktop-v2.md § "Observabilidade":
//! - Log lines are always English (no i18n on logs).
//! - `CADENZA_LOG` env overrides the level (default `info`).
//! - Token redaction is the responsibility of `auth.rs`; this module
//!   only configures the subscriber.

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
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?;
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env("CADENZA_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let file_layer = fmt::layer().with_writer(non_blocking).with_ansi(false);
    let stderr_layer = fmt::layer().with_writer(std::io::stderr).with_ansi(true);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer)
        .init();

    Ok(guard)
}

/// `~/.cadenza/logs/` — falls back to the system temp dir if there's
/// no home directory, so we never panic during early boot.
pub fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".cadenza")
        .join("logs")
}
