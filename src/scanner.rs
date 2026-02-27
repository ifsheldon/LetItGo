use ignore::{WalkBuilder, WalkState};
use std::{path::PathBuf, sync::Mutex};
use tracing::{debug, warn};

/// Thread-local buffer that merges into a shared `Mutex<Vec<PathBuf>>` on drop.
///
/// Each parallel-walker thread accumulates discovered repos locally, avoiding
/// per-repo lock contention. The merge happens once per thread when the walker
/// callback is dropped.
struct RepoCollector<'a> {
    local: Vec<PathBuf>,
    global: &'a Mutex<Vec<PathBuf>>,
}

impl Drop for RepoCollector<'_> {
    fn drop(&mut self) {
        if !self.local.is_empty() {
            self.global.lock().unwrap().append(&mut self.local);
        }
    }
}

/// Scan `search_paths` for Git repository roots in parallel using the `ignore`
/// crate's parallel walker. Directories listed in `ignored_paths` are skipped.
///
/// Returns a deduplicated list of repo root `PathBuf`s.
pub fn discover_repos(search_paths: &[PathBuf], ignored_paths: &[PathBuf]) -> Vec<PathBuf> {
    let repos: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    for search_root in search_paths {
        if !search_root.exists() {
            warn!("Search path does not exist: {}", search_root.display());
            continue;
        }

        let mut builder = WalkBuilder::new(search_root);
        builder
            .hidden(false) // visit hidden dirs (e.g. .git)
            .follow_links(false) // never follow symlinks
            .git_ignore(false) // we handle ignore logic ourselves
            .git_global(false)
            .git_exclude(false);

        let walker = builder.build_parallel();

        walker.run(|| {
            let mut collector = RepoCollector {
                local: Vec::new(),
                global: &repos,
            };

            Box::new(move |result| {
                let entry = match result {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Walk error: {}", e);
                        return WalkState::Continue;
                    }
                };

                let path = entry.path();

                // Skip configured ignored paths (linear scan; fine for typical ~6 entries)
                if ignored_paths.iter().any(|ig| path.starts_with(ig)) {
                    debug!("Skipping ignored path: {}", path.display());
                    return WalkState::Skip;
                }

                // We're looking for .git entries — either a directory (regular repos)
                // or a file (submodules and worktrees use a file pointing to the
                // actual git dir). WalkState::Skip is a no-op for files.
                if entry.file_name() == ".git"
                    && let Some(repo_root) = path.parent()
                {
                    debug!("Found repo: {}", repo_root.display());
                    collector.local.push(repo_root.to_path_buf());
                    return WalkState::Skip;
                }

                WalkState::Continue
            })
        });
    }

    // Deduplicate (unlikely, but possible if search paths overlap)
    let mut result = repos.into_inner().unwrap();
    result.sort();
    result.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_discover_repos_finds_git_dirs() {
        let tmp = tempdir().unwrap();

        // Create two fake repos
        fs::create_dir_all(tmp.path().join("repo-a/.git")).unwrap();
        fs::create_dir_all(tmp.path().join("repo-b/.git")).unwrap();
        fs::create_dir_all(tmp.path().join("not-a-repo/src")).unwrap();

        let repos = discover_repos(&[tmp.path().to_path_buf()], &[]);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains(&tmp.path().join("repo-a")));
        assert!(repos.contains(&tmp.path().join("repo-b")));
    }

    #[test]
    fn test_discover_repos_skips_ignored() {
        let tmp = tempdir().unwrap();

        fs::create_dir_all(tmp.path().join("keep/repo/.git")).unwrap();
        fs::create_dir_all(tmp.path().join("skip/repo/.git")).unwrap();

        let ignored = vec![tmp.path().join("skip")];
        let repos = discover_repos(&[tmp.path().to_path_buf()], &ignored);

        assert_eq!(repos.len(), 1);
        assert!(repos.contains(&tmp.path().join("keep/repo")));
    }

    #[test]
    fn test_discover_repos_nested_submodules() {
        let tmp = tempdir().unwrap();

        // Outer repo
        fs::create_dir_all(tmp.path().join("outer/.git")).unwrap();
        // Nested submodule
        fs::create_dir_all(tmp.path().join("outer/sub/.git")).unwrap();

        let repos = discover_repos(&[tmp.path().to_path_buf()], &[]);
        // Both outer and sub should be discovered
        assert!(repos.contains(&tmp.path().join("outer")));
        assert!(repos.contains(&tmp.path().join("outer/sub")));
    }

    #[test]
    fn test_discover_repos_missing_search_path() {
        // Should warn but not panic
        let repos = discover_repos(&[PathBuf::from("/nonexistent/path")], &[]);
        assert!(repos.is_empty());
    }

    #[test]
    fn test_discover_repos_multiple_search_paths() {
        let tmp = tempdir().unwrap();
        let dir1 = tmp.path().join("d1");
        let dir2 = tmp.path().join("d2");
        fs::create_dir_all(dir1.join("repo-a/.git")).unwrap();
        fs::create_dir_all(dir2.join("repo-b/.git")).unwrap();

        let repos = discover_repos(&[dir1.clone(), dir2.clone()], &[]);
        assert_eq!(repos.len(), 2);
        assert!(repos.contains(&dir1.join("repo-a")));
        assert!(repos.contains(&dir2.join("repo-b")));
    }

    #[test]
    fn test_discover_repos_finds_git_file_submodule() {
        let tmp = tempdir().unwrap();

        // Simulate a submodule: .git is a file, not a directory
        fs::create_dir_all(tmp.path().join("submod")).unwrap();
        fs::write(
            tmp.path().join("submod/.git"),
            "gitdir: ../../.git/modules/submod\n",
        )
        .unwrap();

        let repos = discover_repos(&[tmp.path().to_path_buf()], &[]);
        assert!(
            repos.contains(&tmp.path().join("submod")),
            "repos with .git files (submodules) should be discovered"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_discover_repos_does_not_follow_symlinks() {
        let tmp = tempdir().unwrap();

        // Real repo lives outside the search path
        let real_repo = tmp.path().join("real-repo");
        fs::create_dir_all(real_repo.join(".git")).unwrap();

        // A symlink inside the search dir that points at the real repo
        let search_dir = tmp.path().join("search");
        fs::create_dir_all(&search_dir).unwrap();
        std::os::unix::fs::symlink(&real_repo, search_dir.join("linked")).unwrap();

        let repos = discover_repos(&[search_dir.clone()], &[]);
        // The symlink must NOT be followed — no repos found
        assert!(repos.is_empty(), "symlinks should not be traversed");
    }
}
