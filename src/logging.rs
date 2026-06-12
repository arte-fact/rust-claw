use std::path::Path;
use std::sync::Arc;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

use crate::logs::{DEFAULT_CAPACITY, LogBuffer, LogLayer};

/// Wires up tracing: stderr + `logs/claw.log` + an in-memory ring the admin log
/// viewer reads (M13). The returned buffer is the one the added layer fills.
pub fn init(logs_dir: &Path) -> anyhow::Result<(WorkerGuard, Arc<LogBuffer>)> {
    std::fs::create_dir_all(logs_dir)?;
    let log_file = tracing_appender::rolling::never(logs_dir, "claw.log");
    let (file_writer, guard) = tracing_appender::non_blocking(log_file);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let buffer = LogBuffer::new(DEFAULT_CAPACITY);
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(fmt::layer().with_ansi(false).with_writer(file_writer))
        .with(LogLayer::new(buffer.clone()))
        .try_init()?;
    Ok((guard, buffer))
}
