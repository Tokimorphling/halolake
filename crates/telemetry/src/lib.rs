//! Process-level telemetry: stderr logs always; OTLP when endpoint is configured.
//!
//! Env:
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` — e.g. `http://127.0.0.1:4317` (enables export)
//! - `OTEL_SERVICE_NAME` — overrides `service_name` when set
//! - `OTEL_EXPORTER_OTLP_PROTOCOL` — `grpc` (default) or `http/protobuf`
//! - `RUST_LOG` — log filter (`info` default)

use anyhow::{Context, Result};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, SpanExporter, WithExportConfig};
use opentelemetry_sdk::{
    Resource, logs::SdkLoggerProvider, metrics::SdkMeterProvider,
    propagation::TraceContextPropagator, trace::SdkTracerProvider,
};
use tracing_subscriber::{EnvFilter, Registry, layer::SubscriberExt, util::SubscriberInitExt};

/// Holds providers so they can be shut down cleanly on drop.
pub struct TelemetryGuard {
    tracer_provider: Option<SdkTracerProvider>,
    logger_provider: Option<SdkLoggerProvider>,
    meter_provider:  Option<SdkMeterProvider>,
    _log_guard:      tracing_appender::non_blocking::WorkerGuard,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.tracer_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.logger_provider.take() {
            let _ = provider.shutdown();
        }
        if let Some(provider) = self.meter_provider.take() {
            let _ = provider.shutdown();
        }
    }
}

/// Initialize stderr logging and optional OTLP export. Call once per process.
pub fn init(service_name: &str) -> Result<TelemetryGuard> {
    let service_name = std::env::var("OTEL_SERVICE_NAME")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| service_name.to_string());

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let (writer, log_guard) = tracing_appender::non_blocking(std::io::stderr());
    let fmt_layer = tracing_subscriber::fmt::layer().with_writer(writer);

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(endpoint) = endpoint.as_deref() {
        let resource = Resource::builder()
            .with_service_name(service_name.clone())
            .build();
        let protocol = std::env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
            .unwrap_or_else(|_| "grpc".into())
            .to_ascii_lowercase();

        let (tracer_provider, logger_provider, meter_provider) =
            install_otlp(endpoint, &protocol, resource)
                .with_context(|| format!("install OTLP exporters at {endpoint}"))?;

        opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
        opentelemetry::global::set_tracer_provider(tracer_provider.clone());
        opentelemetry::global::set_meter_provider(meter_provider.clone());

        let tracer = tracer_provider.tracer(service_name.clone());
        let otel_trace_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let otel_log_layer = OpenTelemetryTracingBridge::new(&logger_provider);

        Registry::default()
            .with(filter)
            .with(fmt_layer)
            .with(otel_trace_layer)
            .with(otel_log_layer)
            .try_init()
            .context("init tracing subscriber with OTLP")?;

        tracing::info!(
            %endpoint,
            protocol = %protocol,
            service = %service_name,
            "OpenTelemetry OTLP export enabled"
        );

        Ok(TelemetryGuard {
            tracer_provider: Some(tracer_provider),
            logger_provider: Some(logger_provider),
            meter_provider:  Some(meter_provider),
            _log_guard:      log_guard,
        })
    } else {
        Registry::default()
            .with(filter)
            .with(fmt_layer)
            .try_init()
            .context("init tracing subscriber")?;

        Ok(TelemetryGuard {
            tracer_provider: None,
            logger_provider: None,
            meter_provider:  None,
            _log_guard:      log_guard,
        })
    }
}

fn install_otlp(
    endpoint: &str,
    protocol: &str,
    resource: Resource,
) -> Result<(SdkTracerProvider, SdkLoggerProvider, SdkMeterProvider)> {
    let use_http = protocol.contains("http");

    let span_exporter = if use_http {
        SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP HTTP span exporter")?
    } else {
        SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP gRPC span exporter")?
    };

    let log_exporter = if use_http {
        LogExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP HTTP log exporter")?
    } else {
        LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP gRPC log exporter")?
    };

    let metric_exporter = if use_http {
        MetricExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP HTTP metric exporter")?
    } else {
        MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
            .context("build OTLP gRPC metric exporter")?
    };

    let tracer_provider = SdkTracerProvider::builder()
        .with_batch_exporter(span_exporter)
        .with_resource(resource.clone())
        .build();

    let logger_provider = SdkLoggerProvider::builder()
        .with_batch_exporter(log_exporter)
        .with_resource(resource.clone())
        .build();

    let meter_provider = SdkMeterProvider::builder()
        .with_periodic_exporter(metric_exporter)
        .with_resource(resource)
        .build();

    Ok((tracer_provider, logger_provider, meter_provider))
}
