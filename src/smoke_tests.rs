// ─── Smoke tests (real tmutil — macOS only) ──────────────────────────────────
//
// These tests are #[ignore]d by default.  They call real `/usr/bin/tmutil` and
// verify xattrs on the filesystem.  Run them with:
//
//     LETITGO_SMOKE=1 cargo test -- --ignored --test-threads=1
//
// CI runs them on macOS runners.  `--test-threads=1` prevents parallel tmutil
// calls from racing.

use super::*;
use crate::config::ExclusionMode;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

// ── Helpers ──────────────────────────────────────────────────────────────

/// Guard: skip if LETITGO_SMOKE != "1".  Combined with `#[ignore]`, this
/// provides a double gate so that `cargo test -- --ignored` on a non-macOS
/// machine (or without tmutil) doesn't blow up.
fn require_smoke() -> bool {
    std::env::var("LETITGO_SMOKE").as_deref() == Ok("1")
}

/// Create an AppContext backed by the **real** `TmutilManager`.
fn make_real_ctx(tmp: &Path) -> AppContext {
    AppContext {
        config_path: tmp.join("config.toml"),
        cache_path: tmp.join("cache.json"),
        lock_path: tmp.join("letitgo.lock"),
        exclusion_manager: Box::new(TmutilManager),
    }
}

/// Check whether `com.apple.metadata:com_apple_backup_excludeItem` xattr
/// is set on `path`.  Uses `/usr/bin/xattr` directly instead of
/// `tmutil isexcluded` because the latter inherits from parent dirs (and
/// CI runners pre-exclude $HOME).
fn has_xattr(path: &Path) -> bool {
    let output = Command::new("/usr/bin/xattr")
        .arg(path)
        .output()
        .expect("failed to run /usr/bin/xattr");
    String::from_utf8_lossy(&output.stdout).contains("com_apple_backup_excludeItem")
}

/// Create a minimal git repo fixture with `target/` and `node_modules/`.
fn make_repo(root: &Path, name: &str) -> PathBuf {
    let repo = root.join(name);
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("target/debug")).unwrap();
    fs::create_dir_all(repo.join("target/release")).unwrap();
    fs::create_dir_all(repo.join("node_modules/lodash")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
    repo
}

/// Build a `Config` for a given search path with optional whitelist globs.
fn smoke_config(search: &Path, whitelist: &[&str]) -> Config {
    Config {
        search_paths: vec![search.to_string_lossy().to_string()],
        ignored_paths: vec![],
        whitelist: whitelist.iter().map(|s| s.to_string()).collect(),
        exclusion_mode: ExclusionMode::Sticky,
    }
}

/// Run `cmd_reset` to clean up real exclusions (best-effort).
fn cleanup(ctx: &AppContext, config: &Config) {
    let _ = cmd_reset(ctx, config, true, false);
}

// ── Core smoke tests ─────────────────────────────────────────────────────

#[test]
#[ignore]
fn smoke_core_dry_run_no_xattrs() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], true /* dry_run */).unwrap();

    assert!(
        !has_xattr(&repo.join("target")),
        "dry-run must not set xattr on target/"
    );
    assert!(
        !has_xattr(&repo.join("node_modules")),
        "dry-run must not set xattr on node_modules/"
    );
}

#[test]
#[ignore]
fn smoke_core_run_sets_xattrs() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(has_xattr(&repo.join("target")), "target/ should have xattr");
    assert!(
        has_xattr(&repo.join("node_modules")),
        "node_modules/ should have xattr"
    );
    let cache = load_cache(&ctx.cache_path).unwrap();
    assert!(cache.path_set().contains(&repo.join("target")));
    assert!(cache.path_set().contains(&repo.join("node_modules")));

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_core_list_json_count() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    let cache = load_cache(&ctx.cache_path).unwrap();
    assert!(
        cache.paths.len() >= 2,
        "expected >= 2 paths, got {}",
        cache.paths.len()
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_core_clean_noop() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();
    let count_before = load_cache(&ctx.cache_path).unwrap().paths.len();

    cmd_clean(&ctx, &config, false).unwrap();
    let count_after = load_cache(&ctx.cache_path).unwrap().paths.len();
    assert_eq!(
        count_before, count_after,
        "clean should not change anything when no paths are stale"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_core_idempotent_rerun() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();
    cmd_run(&ctx, &config, &[], false).unwrap(); // second run

    assert!(
        has_xattr(&repo.join("target")),
        "xattr must survive idempotent re-run"
    );
    assert!(
        has_xattr(&repo.join("node_modules")),
        "xattr must survive idempotent re-run"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_core_reset_removes_xattrs() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(has_xattr(&repo.join("target")), "precondition: xattr set");

    cmd_reset(&ctx, &config, true, false).unwrap();

    assert!(
        !has_xattr(&repo.join("target")),
        "reset must remove xattr from target/"
    );
    assert!(
        !has_xattr(&repo.join("node_modules")),
        "reset must remove xattr from node_modules/"
    );
    assert!(
        !ctx.cache_path.exists(),
        "cache must be deleted after reset"
    );
}

// ── .lignore + whitelist smoke tests ─────────────────────────────────────

#[test]
#[ignore]
fn smoke_lignore_negation() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    fs::write(repo.join(".lignore"), "!target/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(
        !has_xattr(&repo.join("target")),
        ".lignore negated target/ — must not have xattr"
    );
    assert!(
        has_xattr(&repo.join("node_modules")),
        "node_modules/ still excluded"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_lignore_addition() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::create_dir_all(repo.join("data/large")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    fs::write(repo.join(".lignore"), "data/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(
        has_xattr(&repo.join("target")),
        "target/ excluded via .gitignore"
    );
    assert!(
        has_xattr(&repo.join("data")),
        "data/ excluded via .lignore addition"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_lignore_combined() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    fs::create_dir_all(repo.join("data")).unwrap();
    fs::write(repo.join(".lignore"), "!target/\ndata/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(!has_xattr(&repo.join("target")), "target/ negated");
    assert!(
        has_xattr(&repo.join("node_modules")),
        "node_modules/ still excluded"
    );
    assert!(has_xattr(&repo.join("data")), "data/ added by .lignore");

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_lignore_nested() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    fs::create_dir_all(repo.join("src/generated")).unwrap();
    fs::write(repo.join("src/.lignore"), "generated/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(has_xattr(&repo.join("target")), "target/ via .gitignore");
    assert!(
        has_xattr(&repo.join("src/generated")),
        "src/generated/ via nested .lignore"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_lignore_whitelist() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join(".env"), "SECRET=1").unwrap();
    fs::write(repo.join(".gitignore"), "target/\n.env\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &["**/.env"]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(has_xattr(&repo.join("target")), "target/ excluded");
    assert!(
        !has_xattr(&repo.join(".env")),
        ".env whitelisted — must not have xattr"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_lignore_whitelist_multi() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::create_dir_all(repo.join("config")).unwrap();
    fs::write(repo.join(".env"), "SECRET=1").unwrap();
    fs::write(repo.join("config/application.yml"), "db: ...").unwrap();
    fs::write(repo.join(".gitignore"), "target/\n.env\nconfig/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &["**/.env", "**/application.yml"]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(has_xattr(&repo.join("target")), "target/ excluded");
    assert!(!has_xattr(&repo.join(".env")), ".env whitelisted");
    assert!(
        !has_xattr(&repo.join("config/application.yml")),
        "application.yml whitelisted"
    );

    cleanup(&ctx, &config);
}

// ── Advanced smoke tests ─────────────────────────────────────────────────

#[test]
#[ignore]
fn smoke_advanced_incremental() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    // First run
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(has_xattr(&repo.join("target")), "target/ after first run");

    // Add dist/
    fs::create_dir_all(repo.join("dist")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\ndist/\n").unwrap();

    // Second run — incremental
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(has_xattr(&repo.join("target")), "target/ still excluded");
    assert!(has_xattr(&repo.join("dist")), "dist/ added incrementally");

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_stale_cleanup() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();
    let before = load_cache(&ctx.cache_path).unwrap().paths.len();

    // Delete one excluded dir
    fs::remove_dir_all(repo.join("node_modules")).unwrap();

    // Clean should remove the stale entry
    cmd_clean(&ctx, &config, false).unwrap();
    let after = load_cache(&ctx.cache_path).unwrap().paths.len();
    assert!(
        after < before,
        "clean must reduce count: {} -> {}",
        before,
        after
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_multi_repo() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("projects");

    let alpha = root.join("alpha");
    fs::create_dir_all(alpha.join(".git")).unwrap();
    fs::create_dir_all(alpha.join("target")).unwrap();
    fs::write(alpha.join(".gitignore"), "target/\n").unwrap();

    let beta = root.join("beta");
    fs::create_dir_all(beta.join(".git")).unwrap();
    fs::create_dir_all(beta.join("build")).unwrap();
    fs::write(beta.join(".gitignore"), "build/\n").unwrap();

    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(&root, &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(has_xattr(&alpha.join("target")), "alpha/target/ excluded");
    assert!(has_xattr(&beta.join("build")), "beta/build/ excluded");

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_nested_gitignore() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("src/vendor")).unwrap();
    fs::create_dir_all(repo.join("src/main")).unwrap();
    fs::write(repo.join(".gitignore"), "").unwrap();
    fs::write(repo.join("src/.gitignore"), "vendor/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(
        has_xattr(&repo.join("src/vendor")),
        "src/vendor/ excluded by nested .gitignore"
    );
    assert!(!has_xattr(&repo.join("src/main")), "src/main/ not excluded");
    assert!(!has_xattr(&repo.join("src")), "src/ itself not excluded");

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_file_level_pattern() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("logs")).unwrap();
    fs::write(repo.join("logs/debug.log"), "log data").unwrap();
    fs::write(repo.join("logs/app.rs"), "fn main()").unwrap();
    fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(
        has_xattr(&repo.join("logs/debug.log")),
        "debug.log excluded by *.log"
    );
    assert!(!has_xattr(&repo.join("logs/app.rs")), "app.rs not excluded");
    assert!(!has_xattr(&repo.join("logs")), "logs/ dir not excluded");

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_init() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let ctx = make_real_ctx(tmp.path());

    // init creates config
    cmd_init(&ctx, false).unwrap();
    assert!(ctx.config_path.exists(), "config must be created");
    let content = fs::read_to_string(&ctx.config_path).unwrap();
    assert!(
        content.contains("search_paths"),
        "config must contain search_paths"
    );

    // init without --force preserves existing
    let original = content.clone();
    cmd_init(&ctx, false).unwrap();
    assert_eq!(
        fs::read_to_string(&ctx.config_path).unwrap(),
        original,
        "init without --force must not overwrite"
    );

    // init with --force overwrites
    fs::write(&ctx.config_path, "# corrupted\n").unwrap();
    cmd_init(&ctx, true).unwrap();
    let restored = fs::read_to_string(&ctx.config_path).unwrap();
    assert!(
        restored.contains("search_paths"),
        "--force must restore content"
    );
}

#[test]
#[ignore]
fn smoke_advanced_submodule() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = tmp.path().join("submod");
    fs::create_dir_all(&repo).unwrap();
    // Submodule: .git is a file, not a directory
    fs::write(repo.join(".git"), "gitdir: /tmp/fake-git-dir\n").unwrap();
    fs::create_dir_all(repo.join("target")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\n").unwrap();
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    cmd_run(&ctx, &config, &[], false).unwrap();

    assert!(
        has_xattr(&repo.join("target")),
        "submodule target/ excluded"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_full_cycle() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    // Run
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(has_xattr(&repo.join("target")), "cycle: run sets xattr");
    assert!(
        has_xattr(&repo.join("node_modules")),
        "cycle: run sets xattr"
    );

    // Reset
    cmd_reset(&ctx, &config, true, false).unwrap();
    assert!(
        !has_xattr(&repo.join("target")),
        "cycle: reset clears xattr"
    );
    assert!(
        !has_xattr(&repo.join("node_modules")),
        "cycle: reset clears xattr"
    );

    // Re-run
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(
        has_xattr(&repo.join("target")),
        "cycle: re-run restores xattr"
    );
    assert!(
        has_xattr(&repo.join("node_modules")),
        "cycle: re-run restores xattr"
    );

    cleanup(&ctx, &config);
}

#[test]
#[ignore]
fn smoke_advanced_removal() {
    if !require_smoke() {
        return;
    }
    let tmp = tempdir().unwrap();
    let repo = make_repo(tmp.path(), "repo");
    let ctx = make_real_ctx(tmp.path());
    let config = smoke_config(tmp.path(), &[]);

    // First run — both excluded
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(has_xattr(&repo.join("target")), "target/ excluded");
    assert!(
        has_xattr(&repo.join("node_modules")),
        "node_modules/ excluded"
    );

    // Drop target/ from .gitignore
    fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();

    // Second run — target/ un-excluded
    cmd_run(&ctx, &config, &[], false).unwrap();
    assert!(
        !has_xattr(&repo.join("target")),
        "target/ un-excluded after pattern removed"
    );
    assert!(
        has_xattr(&repo.join("node_modules")),
        "node_modules/ still excluded"
    );

    cleanup(&ctx, &config);
}
