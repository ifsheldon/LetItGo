use anyhow::{Context, Result};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{debug, warn};

use crate::error::is_tmutil_safe_error;

/// Abstraction over Time Machine exclusion operations.
///
/// Required to be `Send + Sync` so that `AppContext` (which owns
/// `Box<dyn ExclusionManager>`) can be referenced across rayon scopes.
/// The production implementation calls `/usr/bin/tmutil`; tests use
/// [`MockExclusionManager`] to avoid touching the system.
pub trait ExclusionManager: Send + Sync {
    /// Add Time Machine exclusions for each path in `paths`.
    ///
    /// When `fixed_path` is `true`, passes the `-p` flag to `tmutil`, which
    /// registers the paths in the system plist instead of setting xattrs.
    /// Paths are processed in chunks of 200 to stay within `ARG_MAX`.
    fn add_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()>;

    /// Remove Time Machine exclusions for each path in `paths`.
    ///
    /// Mirrors [`add_exclusions`](Self::add_exclusions) — `fixed_path` must
    /// match the mode that was used when the exclusion was originally added.
    fn remove_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()>;

    /// Return `true` if `path` is currently excluded from Time Machine backups.
    ///
    /// Implemented by running `tmutil isexcluded` and checking for `[Excluded]`
    /// in stdout.
    fn is_excluded(&self, path: &Path) -> Result<bool>;
}

/// Blanket impl so `Arc<T>` can be used as an `ExclusionManager` in tests.
/// This lets tests share a `MockExclusionManager` via `Arc` while still
/// satisfying the `Box<dyn ExclusionManager>` requirement on `AppContext`.
#[cfg(test)]
impl<T: ExclusionManager> ExclusionManager for std::sync::Arc<T> {
    fn add_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()> {
        self.as_ref().add_exclusions(paths, fixed_path)
    }
    fn remove_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()> {
        self.as_ref().remove_exclusions(paths, fixed_path)
    }
    fn is_excluded(&self, path: &Path) -> Result<bool> {
        self.as_ref().is_excluded(path)
    }
}

// ─── Production implementation ───────────────────────────────────────────────

/// Calls the real `/usr/bin/tmutil` binary.
pub struct TmutilManager;

impl ExclusionManager for TmutilManager {
    fn add_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for chunk in paths.chunks(200) {
            run_tmutil("addexclusion", chunk, fixed_path)?;
        }
        Ok(())
    }

    fn remove_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for chunk in paths.chunks(200) {
            run_tmutil("removeexclusion", chunk, fixed_path)?;
        }
        Ok(())
    }

    fn is_excluded(&self, path: &Path) -> Result<bool> {
        let output = Command::new("/usr/bin/tmutil")
            .arg("isexcluded")
            .arg(path)
            .output()
            .context("spawning tmutil isexcluded")?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        Ok(stdout.contains("[Excluded]"))
    }
}

/// Invoke `/usr/bin/tmutil <verb> [-p] <paths…>` and handle its exit code.
///
/// Exit code 213 (path not found) is treated as non-fatal and logged as a
/// warning; any other non-zero exit code returns an error.  The `-p` flag is
/// included only when `fixed_path` is `true`.
fn run_tmutil(verb: &str, paths: &[PathBuf], fixed_path: bool) -> Result<()> {
    let mut cmd = Command::new("/usr/bin/tmutil");
    cmd.arg(verb);
    if fixed_path {
        cmd.arg("-p");
    }
    for path in paths {
        cmd.arg(path);
    }

    debug!(
        "tmutil {} {} paths (fixed_path={})",
        verb,
        paths.len(),
        fixed_path
    );

    let output = cmd
        .output()
        .with_context(|| format!("spawning tmutil {verb}"))?;

    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        if is_tmutil_safe_error(code) {
            warn!(
                "tmutil {} exited with code {} (safe, ignored): {}",
                verb,
                code,
                String::from_utf8_lossy(&output.stderr)
            );
        } else {
            anyhow::bail!(
                "tmutil {} failed (exit {}): {}",
                verb,
                code,
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    Ok(())
}

// ─── Mock implementation (test-only) ─────────────────────────────────────────

#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::Mutex;

    /// Records calls in-memory; never touches the system.
    /// Uses `Mutex` (not `RefCell`) to satisfy `Send + Sync`.
    #[derive(Debug, Default)]
    pub struct MockExclusionManager {
        pub added: Mutex<Vec<PathBuf>>,
        pub removed: Mutex<Vec<PathBuf>>,
    }

    impl MockExclusionManager {
        pub fn new() -> Self {
            Self::default()
        }

        /// Return a snapshot of all paths that have been passed to [`add_exclusions`](ExclusionManager::add_exclusions).
        pub fn added_paths(&self) -> Vec<PathBuf> {
            self.added.lock().unwrap().clone()
        }

        /// Return a snapshot of all paths that have been passed to [`remove_exclusions`](ExclusionManager::remove_exclusions).
        pub fn removed_paths(&self) -> Vec<PathBuf> {
            self.removed.lock().unwrap().clone()
        }
    }

    impl ExclusionManager for MockExclusionManager {
        fn add_exclusions(&self, paths: &[PathBuf], _fixed_path: bool) -> Result<()> {
            self.added.lock().unwrap().extend_from_slice(paths);
            Ok(())
        }

        fn remove_exclusions(&self, paths: &[PathBuf], _fixed_path: bool) -> Result<()> {
            self.removed.lock().unwrap().extend_from_slice(paths);
            Ok(())
        }

        fn is_excluded(&self, path: &Path) -> Result<bool> {
            Ok(self.added.lock().unwrap().contains(&path.to_path_buf()))
        }
    }
}
