//! DRGTW gateway binary. WP 1.4: proxy wired in, request-ID middleware.

use std::path::PathBuf;
use std::process;

use clap::Parser;
use drgtw_config::load;
use drgtw::server;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "drgtw", about = "DRGTW LLM gateway")]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(long, short)]
    config: PathBuf,

    /// Validate config and exit without starting the server.
    #[arg(long)]
    validate_config: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let config = match load(&cli.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}");
            let mut source = std::error::Error::source(&e);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = cause.source();
            }
            process::exit(1);
        }
    };

    // Relative model paths resolve against the config file's directory.
    let base_dir = cli
        .config
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    if cli.validate_config {
        // Also validate PII engine construction (custom recognizer regexes
        // compile and the NER model loads here — both fail boot per WP 3.4/4.4).
        if let Err(e) = server::router(std::sync::Arc::new(config), &base_dir) {
            eprintln!("error: {e}");
            process::exit(1);
        }
        println!("config valid");
        return;
    }

    if let Err(e) = server::run(config, &base_dir).await {
        eprintln!("fatal: {e}");
        process::exit(1);
    }
}
