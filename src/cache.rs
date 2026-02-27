use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

use crate::config::ExclusionMode;

/// Persistent state written to disk between runs.
///
/// Stored as pretty-printed JSON at `~/.cache/letitgo/cache.json`.
/// Tracks which paths are currently excluded from Time Machine so that
/// subsequent runs can compute a diff and avoid redundant `tmutil` calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub last_run: Option<DateTime<FixedOffset>>,
    pub exclusion_mode: ExclusionMode,
    pub paths: Vec<PathBuf>,
}

impl Cache {
    /// Create an empty cache with sensible defaults (version 1, sticky mode).
    pub fn empty() -> Self {
        Cache {
            version: 1,
            last_run: None,
            exclusion_mode: ExclusionMode::Sticky,
            paths: Vec::new(),
        }
    }

    /// Return the cached paths as a `HashSet` for O(1) membership tests.
    pub fn path_set(&self) -> HashSet<PathBuf> {
        self.paths.iter().cloned().collect()
    }
}

/// Load the cache from `path`. Returns an empty cache if the file does not exist.
pub fn load_cache(path: &Path) -> Result<Cache> {
    if !path.exists() {
        return Ok(Cache::empty());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("reading cache: {}", path.display()))?;
    let cache: Cache = serde_json::from_str(&text)
        .with_context(|| format!("parsing cache: {}", path.display()))?;
    Ok(cache)
}

/// Write the cache to `path` atomically.
///
/// Serialises to a `NamedTempFile` in the same directory as `path`, then
/// renames it into place. `rename(2)` is atomic on POSIX systems, so a
/// concurrent reader (or a mid-write Ctrl-C / kill) will always see either
/// the old complete file or the new complete file — never a partial write.
pub fn write_cache(path: &Path, cache: &Cache) -> Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .with_context(|| format!("creating cache dir: {}", parent.display()))?;

    let text = serde_json::to_string_pretty(cache).context("serializing cache")?;

    // Write to a sibling temp file, then atomically rename into place.
    let mut tmp = NamedTempFile::new_in(parent)
        .with_context(|| format!("creating temp file in {}", parent.display()))?;
    std::io::Write::write_all(&mut tmp, text.as_bytes())
        .with_context(|| format!("writing cache temp file in {}", parent.display()))?;
    tmp.persist(path)
        .with_context(|| format!("persisting cache to {}", path.display()))?;

    Ok(())
}

/// Compute the diff between two path sets.
///
/// Returns `(to_add, to_remove)` where:
/// - `to_add`    = paths in `new_set` but not in `old_set`
/// - `to_remove` = paths in `old_set` but not in `new_set`
pub fn diff_sets(
    old_set: &HashSet<PathBuf>,
    new_set: &HashSet<PathBuf>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let to_add: Vec<PathBuf> = new_set.difference(old_set).cloned().collect();
    let to_remove: Vec<PathBuf> = old_set.difference(new_set).cloned().collect();
    (to_add, to_remove)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    fn pb(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn test_empty_cache() {
        let cache = Cache::empty();
        assert!(cache.paths.is_empty());
        assert_eq!(cache.version, 1);
    }

    #[test]
    fn test_round_trip() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        let mut cache = Cache::empty();
        cache.paths = vec![pb("/foo/bar"), pb("/baz/qux")];
        write_cache(&path, &cache).unwrap();

        let loaded = load_cache(&path).unwrap();
        assert_eq!(
            loaded.path_set(),
            [pb("/foo/bar"), pb("/baz/qux")].into_iter().collect()
        );
    }

    #[test]
    fn test_load_missing_returns_empty() {
        let cache = load_cache(Path::new("/nonexistent/cache.json")).unwrap();
        assert!(cache.paths.is_empty());
    }

    #[test]
    fn test_diff_sets() {
        let old: HashSet<PathBuf> = [pb("/a"), pb("/b"), pb("/c")].into_iter().collect();
        let new: HashSet<PathBuf> = [pb("/b"), pb("/c"), pb("/d")].into_iter().collect();

        let (to_add, to_remove) = diff_sets(&old, &new);
        assert_eq!(to_add, vec![pb("/d")]);
        assert_eq!(to_remove, vec![pb("/a")]);
    }

    #[test]
    fn test_diff_sets_empty_cache() {
        let old: HashSet<PathBuf> = HashSet::new();
        let new: HashSet<PathBuf> = [pb("/x"), pb("/y")].into_iter().collect();

        let (to_add, to_remove) = diff_sets(&old, &new);
        let mut sorted = to_add.clone();
        sorted.sort();
        assert_eq!(sorted, vec![pb("/x"), pb("/y")]);
        assert!(to_remove.is_empty());
    }

    #[test]
    fn test_write_cache_creates_nested_parent_dirs() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("a/b/c/cache.json");
        write_cache(&nested, &Cache::empty()).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn test_write_cache_overwrites_completely() {
        // Verifies the atomic-overwrite guarantee: after a second write the file
        // must contain *only* the new data — no mixing of old and new bytes.
        // This would fail with a plain truncate+write strategy if the new content
        // were shorter than the old; rename(2) avoids that class of bug entirely.
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("cache.json");

        // First write: a larger payload
        let mut large = Cache::empty();
        large.paths = (0..20)
            .map(|i| PathBuf::from(format!("/old/path/{i}")))
            .collect();
        write_cache(&path, &large).unwrap();

        // Second write: a smaller payload
        let mut small = Cache::empty();
        small.paths = vec![pb("/new/only/path")];
        write_cache(&path, &small).unwrap();

        // Must deserialize cleanly and contain exactly the new data
        let loaded = load_cache(&path).unwrap();
        assert_eq!(loaded.paths, vec![pb("/new/only/path")]);
        // No leftover bytes from the larger first write
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("/old/path/"),
            "old data must not survive an overwrite"
        );
    }

    #[test]
    fn test_diff_sets_identical_sets_no_changes() {
        let set: HashSet<PathBuf> = [pb("/a"), pb("/b")].into_iter().collect();
        let (to_add, to_remove) = diff_sets(&set, &set);
        assert!(to_add.is_empty());
        assert!(to_remove.is_empty());
    }
}
