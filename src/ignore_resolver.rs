use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};
use tracing::{debug, warn};
use walkdir::WalkDir;

/// Resolve the set of paths that should be excluded from Time Machine backups
/// for a single Git repository, applying .gitignore and .lignore rules.
///
/// Uses a single-pass algorithm: `.gitignore` files are discovered incrementally
/// during the walk, and the matcher is rebuilt each time a new one is found.
/// Ignored directories are physically pruned via `skip_current_dir()`, so large
/// trees like `node_modules/` are never traversed.
///
/// Returns a set of absolute `PathBuf`s.
pub fn resolve_excluded_paths(
    repo_root: &Path,
    whitelist_globs: &GlobSet,
) -> Result<HashSet<PathBuf>> {
    let mut excluded: HashSet<PathBuf> = HashSet::new();

    // ---- Single-pass: walk + incremental .gitignore discovery ----

    // Pre-load the root .gitignore (if any) so its rules apply to first-level entries.
    let mut gitignore_files: Vec<PathBuf> = Vec::new();
    let root_gi = repo_root.join(".gitignore");
    if root_gi.exists() {
        gitignore_files.push(root_gi);
    }
    let mut matcher = build_gitignore_from_files(repo_root, &gitignore_files)?;

    // Use a while-let loop so we can call skip_current_dir() for physical pruning.
    let mut walker = WalkDir::new(repo_root)
        .follow_links(false)
        .min_depth(1)
        .into_iter();

    while let Some(entry_result) = walker.next() {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                warn!("Permission error walking {}: {}", repo_root.display(), e);
                continue;
            }
        };

        let path = entry.path();
        let is_dir = entry.file_type().is_dir();

        // Skip .git directories (don't descend)
        if is_dir && path.file_name().is_some_and(|n| n == ".git") {
            walker.skip_current_dir();
            continue;
        }

        // Relative path from repo root, needed by gitignore matching
        let rel = match path.strip_prefix(repo_root) {
            Ok(r) => r,
            Err(_) => continue,
        };

        // Skip if any parent is already in excluded set (logical pruning)
        let mut ancestor_excluded = false;
        for ancestor in path.ancestors().skip(1) {
            if excluded.contains(ancestor) {
                ancestor_excluded = true;
                break;
            }
        }
        if ancestor_excluded {
            if is_dir {
                walker.skip_current_dir();
            }
            continue;
        }

        // For non-ignored directories: check if they contain a .gitignore and
        // rebuild the matcher. This must happen BEFORE the ignore check so that
        // children of this directory see the updated rules.
        if is_dir {
            let gi_path = path.join(".gitignore");
            if gi_path.exists() && !gitignore_files.contains(&gi_path) {
                gitignore_files.push(gi_path);
                matcher = build_gitignore_from_files(repo_root, &gitignore_files)?;
            }
        }

        // Check if this entry is ignored
        match matcher.matched(rel, is_dir) {
            ignore::Match::Ignore(_) => {
                debug!("gitignore match: {}", path.display());
                excluded.insert(path.to_path_buf());
                if is_dir {
                    walker.skip_current_dir(); // physical pruning
                }
            }
            ignore::Match::Whitelist(_) | ignore::Match::None => {
                // Continue walking
            }
        }
    }

    // ---- Apply .lignore overrides ----
    apply_lignore_overrides(repo_root, &mut excluded)?;

    // ---- Apply config whitelist ----
    apply_whitelist(&mut excluded, whitelist_globs);

    Ok(excluded)
}

/// Build a `Gitignore` matcher from a list of `.gitignore` file paths.
///
/// Called each time a new `.gitignore` file is discovered during the walk.
/// The cost is O(N) in the number of files, but N is typically < 10.
fn build_gitignore_from_files(repo_root: &Path, files: &[PathBuf]) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(repo_root);
    for path in files {
        if let Some(err) = builder.add(path) {
            warn!("Error reading {}: {}", path.display(), err);
        }
    }
    builder.build().context("building gitignore matcher")
}

/// Apply `.lignore` override files:
/// - Plain patterns → add to exclusion set
/// - Negated patterns (`!pattern`) → remove from exclusion set (exact match only)
fn apply_lignore_overrides(repo_root: &Path, excluded: &mut HashSet<PathBuf>) -> Result<()> {
    // Find all .lignore files, skipping .git and already-excluded directories.
    let mut walker = WalkDir::new(repo_root)
        .follow_links(false)
        .into_iter();

    while let Some(entry_result) = walker.next() {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                warn!("Walk error discovering .lignore files: {}", e);
                continue;
            }
        };

        let path = entry.path();
        let is_dir = entry.file_type().is_dir();

        if is_dir {
            if path.file_name().is_some_and(|n| n == ".git") || excluded.contains(path) {
                walker.skip_current_dir();
                continue;
            }
        }

        if entry.file_type().is_file() && entry.file_name() == ".lignore" {
            let lignore_path = entry.path();
            let lignore_dir = match lignore_path.parent() {
                Some(d) => d,
                None => continue,
            };
            process_lignore_file(lignore_path, lignore_dir, excluded)?;
        }
    }
    Ok(())
}

/// Parse a single `.lignore` file and update `excluded` accordingly.
///
/// Lines are processed in two passes:
/// 1. **Addition pass** — plain patterns (no leading `!`) are compiled into a
///    gitignore-style matcher and matched against files under `lignore_dir`.
///    Matching paths are inserted into `excluded`.
/// 2. **Negation pass** — patterns prefixed with `!` are resolved to absolute
///    paths relative to `lignore_dir`.  If the resolved path is directly present
///    in `excluded`, it is removed.  Sub-path negation (e.g. `!target/release`
///    when only `target/` is in `excluded`) emits a warning and is not applied.
fn process_lignore_file(
    lignore_path: &Path,
    lignore_dir: &Path,
    excluded: &mut HashSet<PathBuf>,
) -> Result<()> {
    let content = match std::fs::read_to_string(lignore_path) {
        Ok(c) => c,
        Err(e) => {
            warn!("Cannot read {}: {}", lignore_path.display(), e);
            return Ok(());
        }
    };

    // We'll build two gitignore matchers: one for additions, one we parse
    // manually for negations. The `ignore` gitignore builder strips the `!`
    // prefix automatically, so we need to read lines ourselves to distinguish.

    let mut addition_builder = GitignoreBuilder::new(lignore_dir);
    let mut negation_patterns: Vec<String> = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        // Skip comments and empty lines
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(neg) = line.strip_prefix('!') {
            negation_patterns.push(neg.to_string());
        } else {
            // Plain addition pattern
            addition_builder.add_line(None, line).with_context(|| {
                format!(
                    "adding lignore line '{line}' from {}",
                    lignore_path.display()
                )
            })?;
        }
    }

    // Apply additions
    let addition_matcher = addition_builder.build()?;
    // Walk lignore_dir to find newly matched paths, skipping .git and excluded dirs.
    let mut add_walker = WalkDir::new(lignore_dir)
        .follow_links(false)
        .min_depth(1)
        .into_iter();

    while let Some(entry_result) = add_walker.next() {
        let entry = match entry_result {
            Ok(e) => e,
            Err(e) => {
                warn!("Walk error in .lignore addition scan: {}", e);
                continue;
            }
        };

        let path = entry.path();
        let is_dir = entry.file_type().is_dir();

        if is_dir && (path.file_name().is_some_and(|n| n == ".git") || excluded.contains(path)) {
            add_walker.skip_current_dir();
            continue;
        }

        let rel = match path.strip_prefix(lignore_dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if let ignore::Match::Ignore(_) = addition_matcher.matched(rel, is_dir) {
            debug!("lignore addition: {}", path.display());
            excluded.insert(path.to_path_buf());
            if is_dir {
                add_walker.skip_current_dir();
            }
        }
    }

    // Apply negations
    for neg_pattern in &negation_patterns {
        // Resolve to absolute path relative to the lignore_dir
        let candidate = lignore_dir.join(neg_pattern.trim_end_matches('/'));

        if excluded.contains(&candidate) {
            // Direct entry — remove it
            debug!("lignore negation removes: {}", candidate.display());
            excluded.remove(&candidate);
        } else {
            // Check if the negated path is a sub-path of an excluded directory
            let is_sub_path = excluded.iter().any(|excl| candidate.starts_with(excl));
            if is_sub_path {
                // Find the parent exclusion for the warning message
                let parent_excl = excluded
                    .iter()
                    .find(|excl| candidate.starts_with(excl.as_path()))
                    .cloned();
                if let Some(parent) = parent_excl {
                    warn!(
                        ".lignore negation `!{}` targets a sub-path of excluded directory `{}`. \
                         Sub-path negation is not yet supported — `{}` remains fully excluded. \
                         Workaround: use `!{}` to fully un-exclude the directory.",
                        neg_pattern,
                        parent.display(),
                        parent.display(),
                        parent.file_name().unwrap_or_default().to_string_lossy(),
                    );
                }
            }
            // Else: the pattern simply doesn't match anything — silently ignore
        }
    }

    Ok(())
}

/// Remove any paths in `excluded` that match at least one glob in `whitelist_globs`.
fn apply_whitelist(excluded: &mut HashSet<PathBuf>, whitelist_globs: &GlobSet) {
    if whitelist_globs.is_empty() {
        return;
    }
    excluded.retain(|path| {
        if whitelist_globs.is_match(path) {
            debug!("whitelist retains: {}", path.display());
            false // remove from exclusion set
        } else {
            true
        }
    });
}

/// Build a `GlobSet` from a list of glob pattern strings.
pub fn build_whitelist_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).with_context(|| format!("invalid whitelist glob: {pattern}"))?;
        builder.add(glob);
    }
    builder.build().context("building whitelist globset")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn make_repo(root: &Path) -> PathBuf {
        let repo = root.join("test-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(repo.join("target/debug")).unwrap();
        fs::create_dir_all(repo.join("target/release")).unwrap();
        fs::create_dir_all(repo.join("node_modules/foo")).unwrap();
        fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
        repo
    }

    fn empty_whitelist() -> GlobSet {
        build_whitelist_globset(&[]).unwrap()
    }

    #[test]
    fn test_basic_gitignore_exclusion() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        assert!(excluded.contains(&repo.join("target")));
        assert!(excluded.contains(&repo.join("node_modules")));
        // sub-paths of excluded dirs should NOT be individually tracked
        assert!(!excluded.contains(&repo.join("target/debug")));
        // non-ignored paths should not be there
        assert!(!excluded.contains(&repo.join("src")));
    }

    #[test]
    fn test_lignore_addition() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        fs::create_dir_all(repo.join("data")).unwrap();
        fs::write(repo.join(".lignore"), "data/\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        assert!(excluded.contains(&repo.join("data")));
        assert!(excluded.contains(&repo.join("target")));
    }

    #[test]
    fn test_lignore_negation_removes_path() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        // Negate top-level `target/` itself (exact match)
        fs::write(repo.join(".lignore"), "!target/\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // target/ should have been removed by the negation
        assert!(!excluded.contains(&repo.join("target")));
        // node_modules/ should still be excluded
        assert!(excluded.contains(&repo.join("node_modules")));
    }

    #[test]
    fn test_lignore_sub_path_negation_warns_but_keeps_parent() {
        // `!target/release/` while `target/` is the exclusion entry
        // Should keep `target/` (and emit a warning, which we just verify doesn't panic)
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        fs::write(repo.join(".lignore"), "!target/release\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // target/ should still be excluded
        assert!(excluded.contains(&repo.join("target")));
    }

    #[test]
    fn test_whitelist_removes_path() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        // Add .env to be excluded via lignore
        fs::write(repo.join(".env"), "SECRET=123").unwrap();
        fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n.env\n").unwrap();

        let wl = build_whitelist_globset(&["**/.env".to_string()]).unwrap();
        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // .env matched whitelist → should NOT be excluded
        assert!(!excluded.contains(&repo.join(".env")));
        // target still excluded
        assert!(excluded.contains(&repo.join("target")));
    }

    #[test]
    fn test_empty_lignore_has_no_effect() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        fs::write(repo.join(".lignore"), "").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        assert!(excluded.contains(&repo.join("target")));
        assert!(excluded.contains(&repo.join("node_modules")));
    }

    #[test]
    fn test_git_dir_itself_not_in_exclusions() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // .git should never appear in the exclusion set
        assert!(!excluded.contains(&repo.join(".git")));
    }

    #[test]
    fn test_file_level_gitignore_pattern() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("test-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("logs")).unwrap();
        fs::write(repo.join("logs/debug.log"), "").unwrap();
        fs::write(repo.join("logs/app.rs"), "").unwrap();
        fs::write(repo.join(".gitignore"), "*.log\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // Only the .log file is excluded, not its parent dir or other files
        assert!(excluded.contains(&repo.join("logs/debug.log")));
        assert!(!excluded.contains(&repo.join("logs/app.rs")));
        assert!(!excluded.contains(&repo.join("logs")));
    }

    #[test]
    fn test_nested_gitignore_scoped_to_subdirectory() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("test-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src/vendor")).unwrap();
        fs::create_dir_all(repo.join("src/main")).unwrap();
        // Root .gitignore is empty; only src/.gitignore has patterns
        fs::write(repo.join(".gitignore"), "").unwrap();
        fs::write(repo.join("src/.gitignore"), "vendor/\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        assert!(excluded.contains(&repo.join("src/vendor")));
        assert!(!excluded.contains(&repo.join("src/main")));
    }

    #[test]
    fn test_nested_lignore_scoped_to_subdirectory() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());
        fs::create_dir_all(repo.join("src/generated")).unwrap();
        fs::write(repo.join("src/.lignore"), "generated/\n").unwrap();
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // Generated dir added by src/.lignore
        assert!(excluded.contains(&repo.join("src/generated")));
        // Existing gitignore patterns still apply
        assert!(excluded.contains(&repo.join("target")));
    }

    #[test]
    fn test_repo_with_no_gitignore_excludes_nothing() {
        let tmp = tempdir().unwrap();
        let repo = tmp.path().join("bare-repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        // No .gitignore at all
        let wl = empty_whitelist();

        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        assert!(excluded.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn test_symlinks_not_followed_inside_repo() {
        let tmp = tempdir().unwrap();
        let repo = make_repo(tmp.path());

        // Directory outside the repo
        let external = tmp.path().join("external");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("secret.txt"), "").unwrap();

        // Symlink inside the repo pointing to the external directory
        std::os::unix::fs::symlink(&external, repo.join("linked-dir")).unwrap();

        let wl = empty_whitelist();
        let excluded = resolve_excluded_paths(&repo, &wl).unwrap();

        // Contents of the external dir must not be reachable via the symlink
        assert!(!excluded.contains(&external.join("secret.txt")));
    }
}
