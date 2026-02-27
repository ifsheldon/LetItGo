use anyhow::{Context, Result};
use std::{
    io::Read,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tracing::{debug, info, warn};

use crate::error::is_tmutil_safe_error;

/// The extended attribute that `tmutil addexclusion` sets in "sticky" mode.
/// Checking for this xattr lets us skip redundant `tmutil` calls for paths
/// that are already excluded.
pub const BACKUP_EXCLUDE_XATTR: &str = "com.apple.metadata:com_apple_backup_excludeItem";

/// Maximum number of paths per `tmutil` subprocess invocation.
///
/// Seems unbatches ones is faster.
const TMUTIL_BATCH_SIZE: usize = 1;

/// How long to wait for a batched `tmutil` subprocess before killing it.
/// Setting xattrs is near-instant; if tmutil takes longer than this the
/// process is likely blocked on a system lock or unresponsive filesystem.
const TMUTIL_BATCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Shorter timeout used when retrying a single path after a batch timeout.
const TMUTIL_SINGLE_TIMEOUT: Duration = Duration::from_secs(5);

/// The value that `tmutil addexclusion` writes into the backup-exclusion xattr.
/// This is a binary plist encoding of the string "com.apple.backupd", exactly
/// matching what `tmutil` produces (verified via `xattr -px` on a real file).
pub const BACKUP_EXCLUDE_XATTR_VALUE: &[u8] = &[
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, // bplist00
    0x5F, 0x10, 0x11, // string type, length 17
    0x63, 0x6F, 0x6D, 0x2E, 0x61, 0x70, 0x70, 0x6C, // com.appl
    0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, // e.backup
    0x64, // d
    0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // plist offset table
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // & trailer
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x1C,
];

/// When `true`, set/remove the backup-exclusion xattr directly instead of
/// spawning `tmutil` subprocesses. This is much faster (pure syscall, no
/// process overhead) and avoids the timeout issues seen with `tmutil`.
/// Set to `false` to fall back to the old `tmutil`-based code path.
const ADD_XATTR_DIRECTLY: bool = true;

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
    /// Paths are processed in small batches with a per-subprocess timeout.
    fn add_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()>;

    /// Remove Time Machine exclusions for each path in `paths`.
    ///
    /// Mirrors [`add_exclusions`](Self::add_exclusions) — `fixed_path` must
    /// match the mode that was used when the exclusion was originally added.
    fn remove_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()>;

    /// Return `true` if `path` is currently excluded from Time Machine backups.
    ///
    /// Implemented by running `tmutil isexcluded` and checking for `[Excluded]`
    /// in stdout.
    fn is_excluded(&self, path: &Path) -> Result<bool>;
}

/// Blanket impl so `Arc<T>` can be used as an `ExclusionManager` in tests.
/// This lets tests share a `MockExclusionManager` via `Arc` while still
/// satisfying the `Box<dyn ExclusionManager>` requirement on `AppContext`.
impl<T: ExclusionManager> ExclusionManager for std::sync::Arc<T> {
    fn add_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()> {
        self.as_ref().add_exclusions(paths, fixed_path)
    }
    fn remove_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()> {
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
    fn add_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()> {
        // In sticky mode (the default), tmutil sets an xattr on each path.
        if !fixed_path {
            // Skip paths that already carry the exclusion xattr.
            let mut already = 0usize;
            let filtered: Vec<&Path> = paths
                .iter()
                .copied()
                .filter(|p| {
                    if has_backup_exclusion(p) {
                        debug!("Already excluded, skipping: {}", p.display());
                        already += 1;
                        false
                    } else {
                        true
                    }
                })
                .collect();

            if already > 0 {
                info!(
                    "Skipped {} already-excluded path(s) ({} remaining)",
                    already,
                    filtered.len()
                );
            }

            if ADD_XATTR_DIRECTLY {
                return set_backup_exclusion_xattr(&filtered);
            }
            return run_tmutil_batched("addexclusion", &filtered, false);
        }

        run_tmutil_batched("addexclusion", paths, fixed_path)
    }

    fn remove_exclusions(&self, paths: &[&Path], fixed_path: bool) -> Result<()> {
        if ADD_XATTR_DIRECTLY && !fixed_path {
            return remove_backup_exclusion_xattr(paths);
        }
        run_tmutil_batched("removeexclusion", paths, fixed_path)
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

/// Check whether `path` already carries the Time Machine backup-exclusion xattr.
///
/// This is a lightweight syscall (no subprocess spawned) that lets us skip
/// redundant `tmutil addexclusion` calls for paths that are already excluded.
/// Returns `false` on any error (e.g. path no longer exists) so we fall
/// through to tmutil which will handle the error appropriately.
fn has_backup_exclusion(path: &Path) -> bool {
    xattr::get(path, BACKUP_EXCLUDE_XATTR)
        .ok()
        .flatten()
        .is_some()
}

/// Set the backup-exclusion xattr directly on each path (no tmutil subprocess).
fn set_backup_exclusion_xattr(paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    info!("Setting exclusion xattr on {} path(s)", paths.len());
    let mut failed = 0usize;
    for path in paths {
        if let Err(e) = xattr::set(path, BACKUP_EXCLUDE_XATTR, BACKUP_EXCLUDE_XATTR_VALUE) {
            warn!("Failed to set xattr on {}: {}", path.display(), e);
            failed += 1;
        }
    }
    if failed > 0 {
        warn!("{} path(s) failed when setting exclusion xattr", failed);
    }
    Ok(())
}

/// Remove the backup-exclusion xattr directly from each path (no tmutil subprocess).
fn remove_backup_exclusion_xattr(paths: &[&Path]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    info!("Removing exclusion xattr from {} path(s)", paths.len());
    let mut failed = 0usize;
    for path in paths {
        if let Err(e) = xattr::remove(path, BACKUP_EXCLUDE_XATTR) {
            // ENOATTR (93) is fine — the xattr was already absent.
            if e.raw_os_error() != Some(93) {
                warn!("Failed to remove xattr from {}: {}", path.display(), e);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        warn!("{} path(s) failed when removing exclusion xattr", failed);
    }
    Ok(())
}

/// Process `paths` through `tmutil <verb>` in small batches with progress
/// logging and a per-subprocess timeout.
///
/// If a batch times out, each path in that batch is retried individually
/// with a shorter timeout.  Only truly problematic paths are skipped.
fn run_tmutil_batched(verb: &str, paths: &[&Path], fixed_path: bool) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }

    let chunks: Vec<&[&Path]> = paths.chunks(TMUTIL_BATCH_SIZE).collect();
    let total = chunks.len();
    let mut timed_out_count: usize = 0;

    for (i, chunk) in chunks.iter().enumerate() {
        info!(
            "tmutil {}: batch {}/{} ({} path(s))",
            verb,
            i + 1,
            total,
            chunk.len()
        );
        if !run_tmutil(verb, chunk, fixed_path, TMUTIL_BATCH_TIMEOUT)? {
            continue; // batch completed successfully
        }

        // Batch timed out — retry each path individually to isolate
        // the problematic one(s) instead of skipping the whole batch.
        warn!(
            "Batch {}/{} timed out — retrying {} path(s) individually",
            i + 1,
            total,
            chunk.len()
        );
        for path in *chunk {
            if run_tmutil(
                verb,
                std::slice::from_ref(path),
                fixed_path,
                TMUTIL_SINGLE_TIMEOUT,
            )? {
                warn!("Skipping timed-out path: {}", path.display());
                timed_out_count += 1;
            }
        }
    }

    if timed_out_count > 0 {
        warn!(
            "tmutil {}: {} path(s) skipped due to timeout",
            verb, timed_out_count
        );
    }

    Ok(())
}

/// Invoke `/usr/bin/tmutil <verb> [-p] <paths…>` with a timeout.
///
/// The subprocess is spawned and polled via `try_wait()`.
/// Returns `Ok(true)` if the call timed out (subprocess was killed),
/// `Ok(false)` if it completed normally.
///
/// Exit code 213 (path not found) is treated as non-fatal.  Any other
/// non-zero exit code returns an error.  The `-p` flag is included only
/// when `fixed_path` is `true`.
fn run_tmutil(verb: &str, paths: &[&Path], fixed_path: bool, timeout: Duration) -> Result<bool> {
    let mut cmd = Command::new("/usr/bin/tmutil");
    cmd.arg(verb);
    if fixed_path {
        cmd.arg("-p");
    }
    for path in paths {
        cmd.arg(path);
    }

    debug!(
        "tmutil {} {} paths (fixed_path={}, timeout={}s)",
        verb,
        paths.len(),
        fixed_path,
        timeout.as_secs()
    );

    let mut child = cmd
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning tmutil {verb}"))?;

    // Poll for completion with timeout
    let start = Instant::now();
    let status = loop {
        match child.try_wait().context("checking tmutil status")? {
            Some(status) => break status,
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait(); // reap zombie
                return Ok(true); // timed out
            }
            None => {
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };

    // Check exit status
    if !status.success() {
        let code = status.code().unwrap_or(-1);
        let stderr_output = child
            .stderr
            .take()
            .map(|mut s| {
                let mut buf = String::new();
                let _ = s.read_to_string(&mut buf);
                buf
            })
            .unwrap_or_default();
        if is_tmutil_safe_error(code) {
            warn!(
                "tmutil {} exited with code {} (safe, ignored): {}",
                verb, code, stderr_output
            );
        } else {
            anyhow::bail!("tmutil {} failed (exit {}): {}", verb, code, stderr_output);
        }
    }

    Ok(false) // completed successfully
}

// ─── Mock implementation (for testing) ───────────────────────────────────────

pub mod mock {
    use super::*;
    use std::path::PathBuf;
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
        fn add_exclusions(&self, paths: &[&Path], _fixed_path: bool) -> Result<()> {
            self.added
                .lock()
                .unwrap()
                .extend(paths.iter().map(|p| p.to_path_buf()));
            Ok(())
        }

        fn remove_exclusions(&self, paths: &[&Path], _fixed_path: bool) -> Result<()> {
            self.removed
                .lock()
                .unwrap()
                .extend(paths.iter().map(|p| p.to_path_buf()));
            Ok(())
        }

        fn is_excluded(&self, path: &Path) -> Result<bool> {
            Ok(self.added.lock().unwrap().contains(&path.to_path_buf()))
        }
    }
}
