//! Process-level telemetry: stderr logs via tracing-subscriber.

use anyhow::{Context, Result};
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt, util::SubscriberInitExt};

/// Holds the log guard so non-blocking stderr writes flush on drop.
pub struct TelemetryGuard {
    _log_guard: tracing_appender::non_blocking::WorkerGuard,
}

/// Initialize stderr logging. Call once per process.
pub fn init(service_name: &str) -> Result<TelemetryGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (writer, log_guard) = tracing_appender::non_blocking(std::io::stderr());
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(writer);

    Registry::default()
        .with(filter)
        .with(fmt_layer)
        .try_init()
        .context("init tracing subscriber")?;

    tracing::info!(%service_name, "tracing initialized");

    Ok(TelemetryGuard {
        _log_guard: log_guard,
    })
}
