use anyhow::Result;
use clap::Parser;

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, env = "HALOLAKE_CONTROL_CONFIG")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = halolake_telemetry::init("halolake-control-api")?;
    let args = Args::parse();
    halolake_control_api::run_from_config_file(&args.config).await
}
