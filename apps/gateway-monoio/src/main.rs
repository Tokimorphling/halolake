use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    #[arg(short, long, default_value = "examples/gateway.toml")]
    config: String,
}

#[monoio::main(timer_enabled = true)]
async fn main() -> Result<()> {
    let (log_writer, _log_flush_guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::fmt()
        .with_writer(log_writer)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    monoio_native_tls::init();

    let args = Args::parse();
    halolake_gateway_monoio::run_from_config_file(&args.config).await
}
