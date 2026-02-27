mod cache;
mod clean;
mod cli;
mod config;
mod error;
mod ignore_resolver;
mod scanner;
mod tmutil;

use anyhow::{Context, Result};
use chrono::Local;
use clap::Parser;
use directories::BaseDirs;
use fd_lock::RwLock as FdRwLock;
use owo_colors::OwoColorize;
use rayon::prelude::*;
use std::{
    collections::HashSet,
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    time::Instant,
};
use tracing::{debug, info, warn};

use cache::{Cache, diff_sets, load_cache, write_cache};
use cli::{Cli, Commands};
use config::{Config, expand_tilde};
use ignore_resolver::{build_whitelist_globset, resolve_excluded_paths};
use scanner::discover_repos;
use tmutil::{ExclusionManager, TmutilManager};

// ─── AppContext ───────────────────────────────────────────────────────────────

/// Shared dependencies injected into every command handler.
///
/// All paths and the [`ExclusionManager`] are owned here so that tests can
/// substitute a [`crate::tmutil::MockExclusionManager`] and use temporary
/// directories without touching the real system state.
pub struct AppContext {
    /// Path to the TOML configuration file.
    pub config_path: PathBuf,
    /// Path to the JSON cache file that records currently-excluded paths.
    pub cache_path: PathBuf,
    /// Path to the advisory lock file used to prevent concurrent runs.
    pub lock_path: PathBuf,
    /// Abstraction over `tmutil` — real in production, mocked in tests.
    pub exclusion_manager: Box<dyn ExclusionManager>,
}

impl AppContext {
    /// Creates the default production context using standard macOS paths.
    fn production() -> Self {
        let config_path = default_config_path();
        let cache_path = default_cache_path();
        let lock_path = cache_path
            .parent()
            .unwrap_or(Path::new("/tmp"))
            .join("letitgo.lock");
        AppContext {
            config_path,
            cache_path,
            lock_path,
            exclusion_manager: Box::new(TmutilManager),
        }
    }
}

fn default_config_path() -> PathBuf {
    expand_tilde("~/.config/letitgo/config.toml")
}

fn default_cache_path() -> PathBuf {
    if let Some(base) = BaseDirs::new() {
        base.cache_dir().join("letitgo/cache.json")
    } else {
        expand_tilde("~/Library/Caches/letitgo/cache.json")
    }
}

// ─── Main entry point ─────────────────────────────────────────────────────────

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

// ─── `run` command ────────────────────────────────────────────────────────────

/// Execute the `run` command: scan repos, compute exclusions, apply the diff.
///
/// Steps: acquire lock → load cache → discover repos → resolve ignored paths
/// → diff against cache → call `tmutil` for additions/removals → write cache.
/// When `dry_run` is `true`, prints what would change but makes no system calls.
fn cmd_run(
    ctx: &AppContext,
    config: &Config,
    search_path_overrides: &[PathBuf],
    dry_run: bool,
) -> Result<()> {
    let start = Instant::now();

    // Acquire lockfile — skip if already held by another instance
    let mut lock = open_lock_file(&ctx.lock_path)?;
    let Ok(_guard) = lock.try_write() else {
        warn!("Another letitgo instance is running. Skipping.");
        return Ok(());
    };

    // Determine effective search paths
    let search_paths: Vec<PathBuf> = if search_path_overrides.is_empty() {
        config.resolved_search_paths()
    } else {
        search_path_overrides.to_vec()
    };
    let ignored_paths = config.resolved_ignored_paths();

    debug!("Scanning {} search path(s)…", search_paths.len());
    for sp in &search_paths {
        debug!("  {}", sp.display());
    }

    // Detect mode switch
    let old_cache = load_cache(&ctx.cache_path)?;
    if old_cache.exclusion_mode != config.exclusion_mode && !old_cache.paths.is_empty() {
        warn!(
            "Exclusion mode changed from `{}` to `{}`. \
             Run `letitgo reset` first to clear old exclusions.",
            old_cache.exclusion_mode, config.exclusion_mode
        );
    }

    let old_set = old_cache.path_set();

    // 1) Discover repos
    let repos = discover_repos(&search_paths, &ignored_paths);
    debug!("Found {} Git repo(s)", repos.len());

    // 2) Build whitelist globset
    let whitelist_globs = build_whitelist_globset(&config.whitelist)?;

    // 3) Resolve excluded paths for each repo in parallel
    let new_set: HashSet<PathBuf> = repos
        .par_iter()
        .map(|repo| resolve_excluded_paths(repo, &whitelist_globs))
        .filter_map(|result| {
            result
                .map_err(|e| warn!("Error resolving paths: {}", e))
                .ok()
        })
        .reduce(HashSet::new, |mut acc, set| {
            acc.extend(set);
            acc
        });

    debug!("Total excluded paths computed: {}", new_set.len());

    // 4) Diff
    let (to_add, to_remove) = diff_sets(&old_set, &new_set);
    let add_count = to_add.len();
    let remove_count = to_remove.len();
    debug!(
        "{} path(s) to add, {} path(s) to remove",
        add_count, remove_count
    );

    // 5) Apply exclusions
    let fixed_path = config.exclusion_mode.is_fixed_path();

    if dry_run {
        for p in &to_add {
            info!("[dry-run] would add exclusion: {}", p.display());
        }
        for p in &to_remove {
            info!("[dry-run] would remove exclusion: {}", p.display());
        }
    } else {
        // Run add and remove in parallel (they're independent)
        let (add_res, remove_res) = std::thread::scope(|s| {
            let add_handle = s.spawn(|| ctx.exclusion_manager.add_exclusions(&to_add, fixed_path));
            let remove_res = ctx
                .exclusion_manager
                .remove_exclusions(&to_remove, fixed_path);
            let add_res = add_handle.join().expect("add thread panicked");
            (add_res, remove_res)
        });

        add_res?;
        remove_res?;

        // Release borrows on old_set/new_set so we can consume new_set.
        drop(to_add);
        drop(to_remove);

        // 6) Write updated cache
        let new_cache = Cache {
            version: 1,
            last_run: Some(Local::now().fixed_offset()),
            exclusion_mode: config.exclusion_mode.clone(),
            paths: new_set.into_iter().collect(),
        };
        write_cache(&ctx.cache_path, &new_cache)?;
    }

    let elapsed = start.elapsed();
    info!(
        "Done in {:.2}s — added {}, removed {}",
        elapsed.as_secs_f64(),
        add_count,
        remove_count,
    );

    Ok(())
}

// ─── `list` command ───────────────────────────────────────────────────────────

/// Execute the `list` command: display paths currently recorded in the cache.
///
/// When `json` is `true`, prints machine-readable JSON on stdout.
/// When `stale_only` is `true`, limits output to paths that no longer exist on disk.
fn cmd_list(ctx: &AppContext, json: bool, stale_only: bool) -> Result<()> {
    let cache = load_cache(&ctx.cache_path)?;
    let use_color = io::stdout().is_terminal();

    if json {
        // Machine-readable JSON on stdout
        let paths_for_output: Vec<&PathBuf> = if stale_only {
            cache.paths.iter().filter(|p| !p.exists()).collect()
        } else {
            cache.paths.iter().collect()
        };

        let output = serde_json::json!({
            "count": paths_for_output.len(),
            "last_run": cache.last_run,
            "exclusion_mode": cache.exclusion_mode,
            "paths": paths_for_output,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    // Plain-text output on stdout
    if stale_only {
        let stale: Vec<&PathBuf> = cache.paths.iter().filter(|p| !p.exists()).collect();
        if stale.is_empty() {
            let msg = "No stale paths found.";
            if use_color {
                println!("{}", msg.dimmed());
            } else {
                println!("{msg}");
            }
        } else {
            let header = format!("{} stale path(s) (no longer exist on disk):", stale.len());
            if use_color {
                println!("{}", header.bold());
            } else {
                println!("{header}");
            }
            println!();
            for p in stale {
                let tag = " [deleted]";
                if use_color {
                    println!("  {}{}", p.display(), tag.yellow());
                } else {
                    println!("  {}{}", p.display(), tag);
                }
            }
        }
    } else {
        let paths = &cache.paths;
        if paths.is_empty() {
            let msg = "No paths excluded from Time Machine.";
            if use_color {
                println!("{}", msg.dimmed());
            } else {
                println!("{msg}");
            }
        } else {
            let header = format!("{} path(s) excluded from Time Machine:", paths.len());
            if use_color {
                println!("{}", header.bold());
            } else {
                println!("{header}");
            }
            println!();
            for p in paths {
                println!("  {}", p.display());
            }
        }
    }

    Ok(())
}

// ─── `reset` command ──────────────────────────────────────────────────────────

/// Execute the `reset` command: remove all managed exclusions and delete the cache.
///
/// Prompts for confirmation unless `yes` is `true`.
/// When `dry_run` is `true`, prints what would be removed but makes no changes.
fn cmd_reset(ctx: &AppContext, config: &Config, yes: bool, dry_run: bool) -> Result<()> {
    // Pre-load to show the count in the confirmation prompt.
    let preview_cache = load_cache(&ctx.cache_path)?;

    if preview_cache.paths.is_empty() {
        info!("Nothing to reset — cache is empty.");
        return Ok(());
    }

    if !yes {
        eprint!(
            "This will remove {} exclusion(s). Continue? [y/N] ",
            preview_cache.paths.len()
        );
        io::stderr().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            info!("Aborted.");
            return Ok(());
        }
    }

    // Acquire lock after confirmation prompt (before any mutations)
    let mut lock = open_lock_file(&ctx.lock_path)?;
    let Ok(_guard) = lock.try_write() else {
        warn!("Another letitgo instance is running. Skipping.");
        return Ok(());
    };

    // Re-load cache under lock to avoid TOCTOU race with concurrent runs.
    let cache = load_cache(&ctx.cache_path)?;
    if cache.paths.is_empty() {
        info!("Nothing to reset — cache is empty.");
        return Ok(());
    }

    let fixed_path = config.exclusion_mode.is_fixed_path();

    if dry_run {
        for p in &cache.paths {
            info!("[dry-run] would remove exclusion: {}", p.display());
        }
    } else {
        let path_refs: Vec<&Path> = cache.paths.iter().map(|p| p.as_path()).collect();
        ctx.exclusion_manager
            .remove_exclusions(&path_refs, fixed_path)?;
        // Delete the cache file
        if ctx.cache_path.exists() {
            fs::remove_file(&ctx.cache_path)
                .with_context(|| format!("removing cache: {}", ctx.cache_path.display()))?;
        }
        info!(
            "Reset complete. {} exclusion(s) removed.",
            cache.paths.len()
        );
    }

    Ok(())
}

// ─── `clean` command ──────────────────────────────────────────────────────────

/// Execute the `clean` command: remove exclusions for paths that no longer exist on disk.
///
/// Delegates to [`clean::clean_stale`].  When `dry_run` is `true`, reports the
/// count of stale paths but does not modify the cache or call `tmutil`.
fn cmd_clean(ctx: &AppContext, config: &Config, dry_run: bool) -> Result<()> {
    // Acquire lock — clean mutates the cache
    let mut lock = open_lock_file(&ctx.lock_path)?;
    let Ok(_guard) = lock.try_write() else {
        warn!("Another letitgo instance is running. Skipping.");
        return Ok(());
    };

    let fixed_path = config.exclusion_mode.is_fixed_path();
    let removed = clean::clean_stale(
        &ctx.cache_path,
        ctx.exclusion_manager.as_ref(),
        fixed_path,
        dry_run,
    )?;
    if removed > 0 {
        info!("Removed {} stale exclusion(s).", removed);
    } else {
        info!("No stale paths found.");
    }
    Ok(())
}

// ─── `init` command ───────────────────────────────────────────────────────────

/// Execute the `init` command: write a default config file with inline comments.
///
/// Does nothing if the file already exists, unless `force` is `true`.
fn cmd_init(ctx: &AppContext, force: bool) -> Result<()> {
    if ctx.config_path.exists() && !force {
        info!(
            "Config file already exists at {}. Use --force to overwrite.",
            ctx.config_path.display()
        );
        return Ok(());
    }

    if let Some(parent) = ctx.config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating config dir: {}", parent.display()))?;
    }

    fs::write(&ctx.config_path, config::DEFAULT_CONFIG)
        .with_context(|| format!("writing config: {}", ctx.config_path.display()))?;

    info!("Config written to {}", ctx.config_path.display());
    Ok(())
}

// ─── Lockfile ─────────────────────────────────────────────────────────────────

/// Open (or create) the lockfile and return it wrapped in an `RwLock`.
///
/// The caller should call [`try_write()`](FdRwLock::try_write) on the returned
/// value and keep the resulting guard alive for the duration of the exclusive
/// section.  Dropping the guard releases the lock; the OS also releases it
/// automatically on process exit (even on `SIGKILL`) because `flock(2)` locks
/// are per-open-file-description.
fn open_lock_file(lock_path: &Path) -> Result<FdRwLock<fs::File>> {
    use std::fs::OpenOptions;

    // Ensure the parent directory exists (lockfile lives next to cache).
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating lockfile dir: {}", parent.display()))?;
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(lock_path)
        .with_context(|| format!("opening lockfile: {}", lock_path.display()))?;

    Ok(FdRwLock::new(file))
}

// ─── Integration tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::config::ExclusionMode;
    use crate::tmutil::mock::MockExclusionManager;
    use std::fs;
    use std::sync::Arc;
    use tempfile::tempdir;

    fn make_ctx(tmp: &Path, mock: MockExclusionManager) -> AppContext {
        AppContext {
            config_path: tmp.join("config.toml"),
            cache_path: tmp.join("cache.json"),
            lock_path: tmp.join("letitgo.lock"),
            exclusion_manager: Box::new(mock),
        }
    }

    /// Create an `AppContext` backed by a shared `Arc<MockExclusionManager>`.
    /// The caller keeps the `Arc` to inspect recorded calls after the command runs.
    fn make_ctx_with_mock(tmp: &Path) -> (AppContext, Arc<MockExclusionManager>) {
        let mock = Arc::new(MockExclusionManager::new());
        let ctx = AppContext {
            config_path: tmp.join("config.toml"),
            cache_path: tmp.join("cache.json"),
            lock_path: tmp.join("letitgo.lock"),
            exclusion_manager: Box::new(Arc::clone(&mock)),
        };
        (ctx, mock)
    }

    fn make_repo(root: &Path, name: &str) -> PathBuf {
        let repo = root.join(name);
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("target/debug")).unwrap();
        fs::create_dir_all(repo.join("node_modules")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        repo
    }

    fn default_config_for_test(search_root: &Path) -> Config {
        Config {
            search_paths: vec![search_root.to_string_lossy().to_string()],
            ignored_paths: vec![],
            whitelist: vec![],
            exclusion_mode: ExclusionMode::Sticky,
        }
    }

    #[test]
    fn test_run_fresh_adds_exclusions() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-a");
        let mock = MockExclusionManager::new();
        let ctx = make_ctx(tmp.path(), mock);
        let config = default_config_for_test(tmp.path());

        cmd_run(&ctx, &config, &[], false).unwrap();

        // Verify via cache
        let cache = load_cache(&ctx.cache_path).unwrap();
        let paths: HashSet<PathBuf> = cache.path_set();

        assert!(paths.contains(&repo.join("target")));
        assert!(paths.contains(&repo.join("node_modules")));
    }

    #[test]
    fn test_run_diff_only_sends_new_paths() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-b");
        let config = default_config_for_test(tmp.path());

        // First run — populates cache
        {
            let mock = MockExclusionManager::new();
            let ctx = make_ctx(tmp.path(), mock);
            cmd_run(&ctx, &config, &[], false).unwrap();
        }

        // Add a new ignored dir
        fs::create_dir_all(repo.join("dist")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\nnode_modules/\ndist/\n").unwrap();

        // Second run — should only diff
        let mock2 = MockExclusionManager::new();
        let ctx2 = make_ctx(tmp.path(), mock2);
        cmd_run(&ctx2, &config, &[], false).unwrap();

        let cache = load_cache(&ctx2.cache_path).unwrap();
        let paths = cache.path_set();
        assert!(paths.contains(&repo.join("dist")));
    }

    #[test]
    fn test_run_lignore_negation_not_added() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-c");
        fs::write(repo.join(".lignore"), "!target/\n").unwrap();
        let config = default_config_for_test(tmp.path());

        let mock = MockExclusionManager::new();
        let ctx = make_ctx(tmp.path(), mock);
        cmd_run(&ctx, &config, &[], false).unwrap();

        let cache = load_cache(&ctx.cache_path).unwrap();
        let paths = cache.path_set();
        // target/ was negated in .lignore — should NOT be excluded
        assert!(!paths.contains(&repo.join("target")));
        // node_modules still excluded
        assert!(paths.contains(&repo.join("node_modules")));
    }

    #[test]
    fn test_clean_removes_stale_paths() {
        let tmp = tempdir().unwrap();
        // Write a cache with a stale path (nonexistent) and a live path (tmp itself)
        let mut cache = Cache::empty();
        cache.paths = vec![
            PathBuf::from("/nonexistent/path/should/not/exist"),
            tmp.path().to_path_buf(),
        ];
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let mock = MockExclusionManager::new();
        let ctx = make_ctx(tmp.path(), mock);

        let removed = clean::clean_stale(
            &ctx.cache_path,
            ctx.exclusion_manager.as_ref(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(removed, 1);

        let updated = load_cache(&ctx.cache_path).unwrap();
        assert_eq!(updated.paths.len(), 1);
        assert_eq!(updated.paths[0], tmp.path());
    }

    #[test]
    fn test_reset_clears_cache() {
        let tmp = tempdir().unwrap();
        let mut cache = Cache::empty();
        cache.paths = vec![tmp.path().join("foo"), tmp.path().join("bar")];
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let mock = MockExclusionManager::new();
        let ctx = make_ctx(tmp.path(), mock);
        let config = Config::default();

        cmd_reset(&ctx, &config, true, false).unwrap();

        assert!(!ctx.cache_path.exists());
    }

    #[test]
    fn test_init_creates_config() {
        let tmp = tempdir().unwrap();
        let mock = MockExclusionManager::new();
        let mut ctx = make_ctx(tmp.path(), mock);
        ctx.config_path = tmp.path().join("sub/config.toml");

        cmd_init(&ctx, false).unwrap();

        assert!(ctx.config_path.exists());
        let content = fs::read_to_string(&ctx.config_path).unwrap();
        assert!(content.contains("search_paths"));
    }

    #[test]
    fn test_init_no_overwrite_without_force() {
        let tmp = tempdir().unwrap();
        let mock = MockExclusionManager::new();
        let mut ctx = make_ctx(tmp.path(), mock);
        ctx.config_path = tmp.path().join("config.toml");
        fs::write(&ctx.config_path, "original").unwrap();

        cmd_init(&ctx, false).unwrap();

        let content = fs::read_to_string(&ctx.config_path).unwrap();
        assert_eq!(content, "original"); // not overwritten
    }

    #[test]
    fn test_init_force_overwrites() {
        let tmp = tempdir().unwrap();
        let mock = MockExclusionManager::new();
        let mut ctx = make_ctx(tmp.path(), mock);
        ctx.config_path = tmp.path().join("config.toml");
        fs::write(&ctx.config_path, "original").unwrap();

        cmd_init(&ctx, true).unwrap();

        let content = fs::read_to_string(&ctx.config_path).unwrap();
        assert!(content.contains("search_paths"));
    }

    // ── run: mock call verification ─────────────────────────────────────────

    #[test]
    fn test_run_calls_add_exclusions_on_mock() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-mock-verify");
        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        let config = default_config_for_test(tmp.path());

        cmd_run(&ctx, &config, &[], false).unwrap();

        let added = mock.added_paths();
        assert!(added.contains(&repo.join("target")));
        assert!(added.contains(&repo.join("node_modules")));
        assert!(mock.removed_paths().is_empty());
    }

    #[test]
    fn test_run_removes_exclusions_for_removed_gitignore_patterns() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-removes");
        let config = default_config_for_test(tmp.path());

        // First run: target/ and node_modules/ both excluded
        {
            let (ctx, _mock) = make_ctx_with_mock(tmp.path());
            cmd_run(&ctx, &config, &[], false).unwrap();
        }

        // Drop target/ from .gitignore
        fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();

        // Second run: target/ must be removed, node_modules/ kept
        {
            let (ctx, mock) = make_ctx_with_mock(tmp.path());
            cmd_run(&ctx, &config, &[], false).unwrap();

            assert!(mock.removed_paths().contains(&repo.join("target")));

            let cache = load_cache(&ctx.cache_path).unwrap();
            assert!(!cache.path_set().contains(&repo.join("target")));
            assert!(cache.path_set().contains(&repo.join("node_modules")));
        }
    }

    // ── run: dry-run ────────────────────────────────────────────────────────

    #[test]
    fn test_run_dry_run_does_not_write_cache() {
        let tmp = tempdir().unwrap();
        make_repo(tmp.path(), "repo-dry");
        let (ctx, _mock) = make_ctx_with_mock(tmp.path());
        let config = default_config_for_test(tmp.path());

        cmd_run(&ctx, &config, &[], true).unwrap();

        assert!(
            !ctx.cache_path.exists(),
            "cache must not be written in dry-run mode"
        );
    }

    #[test]
    fn test_run_dry_run_does_not_call_exclusion_manager() {
        let tmp = tempdir().unwrap();
        make_repo(tmp.path(), "repo-dry-mock");
        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        let config = default_config_for_test(tmp.path());

        cmd_run(&ctx, &config, &[], true).unwrap();

        assert!(
            mock.added_paths().is_empty(),
            "add_exclusions must not be called in dry-run"
        );
        assert!(
            mock.removed_paths().is_empty(),
            "remove_exclusions must not be called in dry-run"
        );
    }

    // ── run: search-path override ───────────────────────────────────────────

    #[test]
    fn test_run_with_search_path_override() {
        let tmp = tempdir().unwrap();
        let alt = tmp.path().join("alt");
        fs::create_dir_all(&alt).unwrap();
        let repo = make_repo(&alt, "repo-override");
        let (ctx, mock) = make_ctx_with_mock(tmp.path());

        // Config points somewhere else entirely
        let config = Config {
            search_paths: vec!["/nonexistent/empty".to_string()],
            ignored_paths: vec![],
            whitelist: vec![],
            exclusion_mode: ExclusionMode::Sticky,
        };

        // Override with the alt dir
        cmd_run(&ctx, &config, &[alt], false).unwrap();

        assert!(mock.added_paths().contains(&repo.join("target")));
    }

    // ── run: mode-switch warning ────────────────────────────────────────────

    #[test]
    fn test_run_mode_switch_does_not_crash() {
        let tmp = tempdir().unwrap();
        make_repo(tmp.path(), "repo-mode");
        let config_sticky = default_config_for_test(tmp.path());

        // First run with sticky
        {
            let (ctx, _mock) = make_ctx_with_mock(tmp.path());
            cmd_run(&ctx, &config_sticky, &[], false).unwrap();
        }

        // Second run with fixed-path — should emit a warning but not crash
        let config_fixed = Config {
            search_paths: vec![tmp.path().to_string_lossy().to_string()],
            ignored_paths: vec![],
            whitelist: vec![],
            exclusion_mode: ExclusionMode::FixedPath,
        };
        {
            let (ctx, _mock) = make_ctx_with_mock(tmp.path());
            cmd_run(&ctx, &config_fixed, &[], false).unwrap();
        }
    }

    // ── run: edge cases ─────────────────────────────────────────────────────

    #[test]
    fn test_run_repo_with_no_gitignore() {
        let tmp = tempdir().unwrap();
        // Repo with a .git dir but no .gitignore
        fs::create_dir_all(tmp.path().join("bare-repo/.git")).unwrap();
        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        let config = default_config_for_test(tmp.path());

        cmd_run(&ctx, &config, &[], false).unwrap();

        assert!(mock.added_paths().is_empty());
    }

    #[test]
    fn test_run_respects_config_whitelist() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path(), "repo-wl");
        fs::write(repo.join(".env"), "SECRET=1").unwrap();
        // Add .env to gitignore so it becomes a candidate for exclusion
        fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n.env\n").unwrap();

        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        let config = Config {
            search_paths: vec![tmp.path().to_string_lossy().to_string()],
            ignored_paths: vec![],
            whitelist: vec!["**/.env".to_string()],
            exclusion_mode: ExclusionMode::Sticky,
        };

        cmd_run(&ctx, &config, &[], false).unwrap();

        let added = mock.added_paths();
        // .env is whitelisted — must NOT be excluded
        assert!(!added.contains(&repo.join(".env")));
        // target/ is still excluded
        assert!(added.contains(&repo.join("target")));
    }

    // ── list ────────────────────────────────────────────────────────────────

    #[test]
    fn test_list_empty_cache_no_error() {
        let tmp = tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), MockExclusionManager::new());
        // All variants must succeed on a missing/empty cache
        cmd_list(&ctx, false, false).unwrap();
        cmd_list(&ctx, true, false).unwrap(); // --json
        cmd_list(&ctx, false, true).unwrap(); // --stale
    }

    #[test]
    fn test_list_with_cached_paths_no_error() {
        let tmp = tempdir().unwrap();
        let mut cache = Cache::empty();
        cache.paths = vec![
            tmp.path().join("live"),                            // exists
            PathBuf::from("/nonexistent/definitely/gone/path"), // stale
        ];
        // The "live" path needs to actually exist for .exists() checks
        fs::create_dir_all(tmp.path().join("live")).unwrap();
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let ctx = make_ctx(tmp.path(), MockExclusionManager::new());

        cmd_list(&ctx, false, false).unwrap();
        cmd_list(&ctx, true, false).unwrap(); // --json
        cmd_list(&ctx, false, true).unwrap(); // --stale: should list the nonexistent path
    }

    // ── reset ───────────────────────────────────────────────────────────────

    #[test]
    fn test_reset_empty_cache_no_error() {
        let tmp = tempdir().unwrap();
        let ctx = make_ctx(tmp.path(), MockExclusionManager::new());
        let config = Config::default();
        // Should print "Nothing to reset" and return Ok — not crash
        cmd_reset(&ctx, &config, true, false).unwrap();
    }

    #[test]
    fn test_reset_dry_run_keeps_cache() {
        let tmp = tempdir().unwrap();
        let mut cache = Cache::empty();
        cache.paths = vec![tmp.path().join("some-path")];
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let ctx = make_ctx(tmp.path(), MockExclusionManager::new());
        let config = Config::default();

        cmd_reset(&ctx, &config, true, true).unwrap(); // yes + dry_run

        assert!(
            ctx.cache_path.exists(),
            "cache must not be deleted in dry-run reset"
        );
    }

    #[test]
    fn test_reset_dry_run_does_not_call_exclusion_manager() {
        let tmp = tempdir().unwrap();
        let mut cache = Cache::empty();
        cache.paths = vec![tmp.path().join("some-path")];
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        let config = Config::default();

        cmd_reset(&ctx, &config, true, true).unwrap();

        assert!(
            mock.removed_paths().is_empty(),
            "remove_exclusions must not be called in dry-run reset"
        );
    }

    // ── clean ───────────────────────────────────────────────────────────────

    #[test]
    fn test_clean_dry_run_does_not_modify_cache() {
        let tmp = tempdir().unwrap();
        let mut cache = Cache::empty();
        cache.paths = vec![PathBuf::from("/nonexistent/stale/path/abc")];
        write_cache(&tmp.path().join("cache.json"), &cache).unwrap();

        let ctx = make_ctx(tmp.path(), MockExclusionManager::new());

        let removed = clean::clean_stale(
            &ctx.cache_path,
            ctx.exclusion_manager.as_ref(),
            false,
            true, // dry_run
        )
        .unwrap();

        // 1 stale path detected
        assert_eq!(removed, 1);
        // But cache must remain unchanged
        let updated = load_cache(&ctx.cache_path).unwrap();
        assert_eq!(updated.paths.len(), 1);
    }

    // ── lockfile concurrency ────────────────────────────────────────────────

    #[test]
    fn test_concurrent_run_skips_when_locked() {
        let tmp = tempdir().unwrap();
        make_repo(tmp.path(), "repo-lock");
        let config = default_config_for_test(tmp.path());

        // Manually hold the lockfile before cmd_run is called
        let lock_path = tmp.path().join("letitgo.lock");
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let mut fd_lock = fd_lock::RwLock::new(lock_file);
        let _guard = fd_lock.try_write().expect("failed to acquire test lock");

        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        // cmd_run must detect the held lock and return Ok() without doing work
        cmd_run(&ctx, &config, &[], false).unwrap();

        // Since the run was skipped, neither the cache nor the mock should have been touched
        assert!(
            !ctx.cache_path.exists(),
            "cache must not be written when run is skipped due to lock"
        );
        assert!(
            mock.added_paths().is_empty(),
            "add_exclusions must not be called when skipped"
        );

        drop(_guard); // release lock explicitly
    }
}
