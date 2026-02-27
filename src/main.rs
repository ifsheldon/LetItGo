use anyhow::{Context, Result};
use clap::Parser;
use tracing::warn;

use letitgo::cli::{Cli, Commands};
use letitgo::config::Config;
use letitgo::{AppContext, cmd_clean, cmd_init, cmd_list, cmd_reset, cmd_run};

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize tracing subscriber based on verbosity flags
    init_tracing(cli.verbose, cli.quiet);

    // Build AppContext — use CLI-override config path if provided
    let mut ctx = AppContext::production();
    if let Some(config_path) = &cli.config {
        ctx.config_path = config_path.clone();
    }

    // Load config (warn on first run if missing)
    let (config, config_found) = Config::load(&ctx.config_path)
        .with_context(|| format!("loading config from {}", ctx.config_path.display()))?;
    if !config_found && !matches!(&cli.command, Commands::Init(_)) {
        warn!(
            "No config file found at {} — using defaults. Run `letitgo init` to create one.",
            ctx.config_path.display()
        );
    }

    let dry_run = cli.dry_run;

    match cli.command {
        Commands::Run(args) => cmd_run(&ctx, &config, &args.search_path, dry_run),
        Commands::List(args) => cmd_list(&ctx, args.json, args.stale),
        Commands::Reset(args) => cmd_reset(&ctx, &config, args.yes, dry_run),
        Commands::Clean => cmd_clean(&ctx, &config, dry_run),
        Commands::Init(args) => cmd_init(&ctx, args.force),
    }
}

// ─── Logging setup ────────────────────────────────────────────────────────────

/// Configure the global `tracing` subscriber based on CLI verbosity flags.
///
/// | flags          | effective level |
/// |----------------|-----------------|
/// | `--quiet`      | `ERROR`         |
/// | *(default)*    | `INFO`          |
/// | `-v`           | `DEBUG`         |
/// | `-vv`          | `TRACE`         |
///
/// The `RUST_LOG` environment variable takes precedence over all flags.
fn init_tracing(verbose: u8, quiet: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let level = if quiet {
        "error"
    } else {
        match verbose {
            0 => "info",
            1 => "debug",
            _ => "trace",
        }
    };

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("letitgo={level}")));

    fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}
