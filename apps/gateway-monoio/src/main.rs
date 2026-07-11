use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, default_value = "examples/gateway.toml")]
    config: String,
}

// Not `#[monoio::main]`: the gateway is thread-per-core. `run_from_config_file`
// spawns one OS thread per worker, each building its own monoio runtime and
// binding the listener with SO_REUSEPORT, so the main thread stays runtime-free.
fn main() -> Result<()> {
    // rustls 0.23 requires an explicit process-level CryptoProvider when
    // multiple providers may be linked via workspace deps.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // OTLP exporters need a Tokio runtime; spawn a background runtime so
    // monoio workers stay free of Tokio. Keep the guard alive for process life.
    let _otel_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .thread_name("halolake-otel")
        .build()?;
    let _telemetry = {
        let _enter = _otel_runtime.enter();
        halolake_telemetry::init("halolake-gateway")?
    };

    monoio_native_tls::init();

    let args = Args::parse();
    halolake_gateway_monoio::run_from_config_file(&args.config)
}
