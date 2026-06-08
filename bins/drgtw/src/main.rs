//! DRGTW gateway binary. WP 1.4: proxy wired in, request-ID middleware.

use std::io::{self, BufRead, Write as _};
use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};
use drgtw::server;
use drgtw_config::load;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Parser)]
#[command(name = "drgtw", about = "DRGTW LLM gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the TOML configuration file. Required for the normal run /
    /// `--validate-config` paths; not needed for subcommands.
    #[arg(long, short)]
    config: Option<PathBuf>,

    /// Validate config and exit without starting the server.
    #[arg(long)]
    validate_config: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Hash a password with argon2id and print the PHC string.
    ///
    /// Use the output as `password_hash` in `[ui.auth]`. Reads from --password
    /// if given, otherwise prompts on stdin.
    HashPassword {
        /// Password to hash. If omitted, read from stdin (one line).
        #[arg(long)]
        password: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Handle subcommands before anything else — they need no config file.
    if let Some(cmd) = cli.command {
        match cmd {
            Command::HashPassword { password } => {
                let pw = match password {
                    Some(p) => p,
                    None => {
                        // Read one line from stdin (supports piping or interactive prompt).
                        print!("Password: ");
                        let _ = io::stdout().flush();
                        let mut line = String::new();
                        io::stdin().lock().read_line(&mut line).unwrap_or(0);
                        line.trim_end_matches('\n').trim_end_matches('\r').to_owned()
                    }
                };
                match drgtw_ui_auth::password::hash_password(&pw) {
                    Ok(phc) => println!("{phc}"),
                    Err(e) => {
                        eprintln!("error: {e}");
                        process::exit(1);
                    }
                }
                return;
            }
        }
    }

    // Normal gateway run path requires --config.
    let Some(config_path) = cli.config else {
        eprintln!("error: --config <PATH> is required (or use a subcommand, e.g. `drgtw hash-password`)");
        process::exit(2);
    };

    // Config is loaded BEFORE the tracing subscriber so the OTel layer can be
    // attached to the same `registry()` when `[otel] enabled`. Load failures
    // are reported via `eprintln!` (no subscriber needed yet).
    let config = match load(&config_path) {
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
    let base_dir = config_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    if cli.validate_config {
        // No subscriber and no OTel exporters for the validate path: just a
        // plain fmt subscriber so any tracing during engine build is visible.
        init_fmt_only();
        // Also validate PII engine construction (custom recognizer regexes
        // compile and the NER model loads here — both fail boot per WP 3.4/4.4).
        if let Err(e) = server::router(std::sync::Arc::new(config), &base_dir, std::path::PathBuf::new()) {
            eprintln!("error: {e}");
            process::exit(1);
        }
        println!("config valid");
        return;
    }

    // Build the OTel guard (traces + metrics providers) when enabled; `None`
    // otherwise. On init failure we fail boot — telemetry misconfig should be
    // loud, consistent with the config-validation rule.
    let otel_guard = match drgtw_otel::init(&config.otel) {
        Ok(g) => g,
        Err(e) => {
            init_fmt_only();
            eprintln!("fatal: failed to initialise OpenTelemetry: {e}");
            process::exit(1);
        }
    };

    // Layered registry: EnvFilter + stderr fmt layer (KEPT) + optional OTel
    // span layer (only when traces are enabled). The `drgtw-trace` JSONL writer
    // and the fmt output are unaffected by the OTel layer's presence.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr));

    match otel_guard.as_ref().and_then(|g| g.tracer_provider()) {
        Some(tp) => registry.with(drgtw_otel::tracer_layer(tp)).init(),
        None => registry.init(),
    }

    // Canonicalise the config path so the UI editor always has an absolute path.
    let config_path = config_path.canonicalize().unwrap_or(config_path);

    if let Err(e) = server::run(config, &base_dir, config_path, otel_guard).await {
        eprintln!("fatal: {e}");
        process::exit(1);
    }
}

/// Install a plain stderr fmt subscriber (no OTel layer). Used by the
/// `--validate-config` path and OTel-init failure path.
fn init_fmt_only() {
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
