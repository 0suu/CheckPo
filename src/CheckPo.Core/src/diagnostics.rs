use crate::{CheckPoError, Result};
use std::path::PathBuf;
use std::sync::Once;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;

static PANIC_HOOK: Once = Once::new();

pub struct DiagnosticsGuard {
    _writer_guard: WorkerGuard,
    pub log_directory: PathBuf,
}

pub fn diagnostic_log_directory() -> Result<PathBuf> {
    Ok(crate::default_storage_root()?.join("diagnostic-logs"))
}

pub fn init_diagnostics() -> Result<DiagnosticsGuard> {
    let log_directory = diagnostic_log_directory()?;
    std::fs::create_dir_all(&log_directory)
        .map_err(|error| crate::io_error(&log_directory, error))?;
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("checkpo")
        .filename_suffix("log")
        // The appender may briefly retain fewer than the maximum, so keep one
        // spare while targeting roughly one week of diagnostics.
        .max_log_files(8)
        .build(&log_directory)
        .map_err(|error| CheckPoError::Unexpected(format!("diagnostic logger: {error}")))?;
    let (writer, writer_guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_writer(writer)
        .try_init()
        .map_err(|error| CheckPoError::Unexpected(format!("diagnostic logger: {error}")))?;

    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            tracing::error!(panic = %panic, "process panic");
            previous(panic);
        }));
    });
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "diagnostics initialized"
    );
    Ok(DiagnosticsGuard {
        _writer_guard: writer_guard,
        log_directory,
    })
}

pub fn log_operation_error(operation: &str, error: &str) {
    tracing::error!(operation, error, "operation failed");
}

pub(crate) fn log_warning(operation: &str, warning: &str) {
    tracing::warn!(operation, warning, "operation warning");
}
