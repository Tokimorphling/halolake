use anyhow::Result;
use clap::Parser;
#[cfg(feature = "allocator-mimalloc")]
use mimalloc::MiMalloc;

#[cfg(feature = "allocator-mimalloc")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

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

    let _telemetry = halolake_telemetry::init("halolake-gateway")?;

    let args = Args::parse();
    halolake_gateway_monoio::run_from_config_file(&args.config)
}
