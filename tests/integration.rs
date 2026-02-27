use letitgo::cache::{Cache, load_cache, write_cache};
use letitgo::clean;
use letitgo::config::{Config, ExclusionMode};
use letitgo::tmutil::mock::MockExclusionManager;
use letitgo::{AppContext, cmd_init, cmd_list, cmd_reset, cmd_run};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
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
fn test_run_mode_switch_skips_in_non_interactive() {
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo-mode");
    let config_sticky = default_config_for_test(tmp.path());

    // First run with sticky — populates cache
    {
        let (ctx, _mock) = make_ctx_with_mock(tmp.path());
        cmd_run(&ctx, &config_sticky, &[], false).unwrap();
    }

    // Second run with fixed-path (non-interactive) — should skip gracefully
    let config_fixed = Config {
        search_paths: vec![tmp.path().to_string_lossy().to_string()],
        ignored_paths: vec![],
        whitelist: vec![],
        exclusion_mode: ExclusionMode::FixedPath,
    };
    {
        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        cmd_run(&ctx, &config_fixed, &[], false).unwrap();

        // Run was skipped — exclusion manager must not have been called
        assert!(mock.added_paths().is_empty());
        assert!(mock.removed_paths().is_empty());
    }
}

#[test]
fn test_run_mode_switch_dry_run_returns_early() {
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo-mode-dry");
    let config_sticky = default_config_for_test(tmp.path());

    // First run with sticky — populates cache
    {
        let (ctx, _mock) = make_ctx_with_mock(tmp.path());
        cmd_run(&ctx, &config_sticky, &[], false).unwrap();
    }

    // Dry-run with fixed-path — should return early without doing work
    let config_fixed = Config {
        search_paths: vec![tmp.path().to_string_lossy().to_string()],
        ignored_paths: vec![],
        whitelist: vec![],
        exclusion_mode: ExclusionMode::FixedPath,
    };
    {
        let (ctx, mock) = make_ctx_with_mock(tmp.path());
        cmd_run(&ctx, &config_fixed, &[], true).unwrap();

        assert!(mock.added_paths().is_empty());
        assert!(mock.removed_paths().is_empty());
    }
}

#[test]
fn test_run_mode_switch_with_empty_cache_proceeds() {
    // When cache is empty, mode switch should NOT block the run
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo-mode-empty");

    let config_fixed = Config {
        search_paths: vec![tmp.path().to_string_lossy().to_string()],
        ignored_paths: vec![],
        whitelist: vec![],
        exclusion_mode: ExclusionMode::FixedPath,
    };
    let (ctx, mock) = make_ctx_with_mock(tmp.path());
    cmd_run(&ctx, &config_fixed, &[], false).unwrap();

    // Should have processed normally — exclusions were added
    assert!(!mock.added_paths().is_empty());
}

#[test]
fn test_run_mode_switch_preserves_old_cache_on_skip() {
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo-mode-preserve");
    let config_sticky = default_config_for_test(tmp.path());

    // First run with sticky — populates cache
    {
        let (ctx, _mock) = make_ctx_with_mock(tmp.path());
        cmd_run(&ctx, &config_sticky, &[], false).unwrap();
    }

    // Verify cache was written with sticky mode
    let cache_before = load_cache(&tmp.path().join("cache.json")).unwrap();
    assert_eq!(cache_before.exclusion_mode, ExclusionMode::Sticky);
    assert!(!cache_before.paths.is_empty());

    // Second run with fixed-path (non-interactive) — should skip
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

    // Cache must be unchanged — still sticky, same paths
    let cache_after = load_cache(&tmp.path().join("cache.json")).unwrap();
    assert_eq!(cache_after.exclusion_mode, ExclusionMode::Sticky);
    assert_eq!(cache_after.paths.len(), cache_before.paths.len());
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
