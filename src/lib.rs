pub mod cache;
pub mod clean;
pub mod cli;
pub mod config;
pub mod error;
pub mod ignore_resolver;
pub mod scanner;
pub mod tmutil;

use anyhow::{Context, Result};
use chrono::Local;
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
use config::{Config, expand_tilde};
use ignore_resolver::{build_whitelist_globset, resolve_excluded_paths};
use scanner::discover_repos;
use tmutil::{ExclusionManager, TmutilManager};

// ─── AppContext ───────────────────────────────────────────────────────────────

/// Shared dependencies injected into every command handler.
///
/// All paths and the [`ExclusionManager`] are owned here so that tests can
/// substitute a [`crate::tmutil::mock::MockExclusionManager`] and use temporary
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
    pub fn production() -> Self {
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

// ─── `run` command ────────────────────────────────────────────────────────────

/// Execute the `run` command: scan repos, compute exclusions, apply the diff.
///
/// Steps: acquire lock → load cache → discover repos → resolve ignored paths
/// → diff against cache → call `tmutil` for additions/removals → write cache.
/// When `dry_run` is `true`, prints what would change but makes no system calls.
pub fn cmd_run(
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

    // Detect mode switch — block the run unless the user resets first
    let old_cache = load_cache(&ctx.cache_path)?;
    if old_cache.exclusion_mode != config.exclusion_mode && !old_cache.paths.is_empty() {
        if dry_run {
            info!(
                "[dry-run] Exclusion mode changed from `{}` to `{}`. \
                 Run `letitgo reset` before switching modes.",
                old_cache.exclusion_mode, config.exclusion_mode
            );
            return Ok(());
        }
        if !io::stdin().is_terminal() {
            warn!(
                "Exclusion mode changed from `{}` to `{}`. \
                 Run `letitgo reset` first to clear old exclusions. Skipping.",
                old_cache.exclusion_mode, config.exclusion_mode
            );
            return Ok(());
        }

        eprint!(
            "Exclusion mode changed from `{}` to `{}`. \
             Do you want to reset now?\n\
             This will remove {} exclusion(s) previously set with `{}` mode \
             and clear the cache, then continue with the scan. [y/N] ",
            old_cache.exclusion_mode,
            config.exclusion_mode,
            old_cache.paths.len(),
            old_cache.exclusion_mode,
        );
        io::stderr().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !matches!(input.trim().to_lowercase().as_str(), "y" | "yes") {
            info!("Aborted. Run `letitgo reset` manually before switching modes.");
            return Ok(());
        }

        // Perform reset using the OLD mode flag so the correct tmutil verb is used
        let old_fixed_path = old_cache.exclusion_mode.is_fixed_path();
        let path_refs: Vec<&Path> = old_cache.paths.iter().map(|p| p.as_path()).collect();
        ctx.exclusion_manager
            .remove_exclusions(&path_refs, old_fixed_path)?;
        if ctx.cache_path.exists() {
            fs::remove_file(&ctx.cache_path).with_context(|| {
                format!("removing cache during reset: {}", ctx.cache_path.display())
            })?;
        }
        info!(
            "Reset complete. {} exclusion(s) removed. Continuing with scan…",
            old_cache.paths.len()
        );
    }

    // Reload cache after potential reset (may now be empty)
    let old_set = load_cache(&ctx.cache_path)?.path_set();

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
pub fn cmd_list(ctx: &AppContext, json: bool, stale_only: bool) -> Result<()> {
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
pub fn cmd_reset(ctx: &AppContext, config: &Config, yes: bool, dry_run: bool) -> Result<()> {
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
pub fn cmd_clean(ctx: &AppContext, config: &Config, dry_run: bool) -> Result<()> {
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
pub fn cmd_init(ctx: &AppContext, force: bool) -> Result<()> {
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
