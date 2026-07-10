use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "HALOLAKE_CONTROL_CONFIG")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let (writer, _guard) = tracing_appender::non_blocking(std::io::stderr());
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(writer)
        .init();
    halolake_control_api::run_from_config_file(&args.config).await
}
