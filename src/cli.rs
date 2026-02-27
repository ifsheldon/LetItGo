use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// letitgo — keep Time Machine backups lean by excluding gitignored paths.
#[derive(Debug, Parser)]
#[command(name = "letitgo", version, about, long_about = None)]
pub struct Cli {
    /// Path to config file
    #[arg(short, long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Increase log verbosity (-v = DEBUG, -vv = TRACE)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress non-error output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Show what would be done without making changes
    #[arg(long, global = true)]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Scan, compute exclusions, and update Time Machine
    Run(RunArgs),

    /// Show currently excluded paths (from cache)
    List(ListArgs),

    /// Remove all exclusions made by letitgo and clear the cache
    Reset(ResetArgs),

    /// Validate cached paths and remove stale exclusions
    Clean,

    /// Create a default config file with inline comments
    Init(InitArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Override configured search paths (repeatable)
    #[arg(long, value_name = "DIR", action = clap::ArgAction::Append)]
    pub search_path: Vec<PathBuf>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Output as JSON
    #[arg(long)]
    pub json: bool,

    /// Show only paths that no longer exist on disk
    #[arg(long)]
    pub stale: bool,
}

#[derive(Debug, Args)]
pub struct ResetArgs {
    /// Skip confirmation prompt
    #[arg(long)]
    pub yes: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Overwrite existing config file
    #[arg(long)]
    pub force: bool,
}
