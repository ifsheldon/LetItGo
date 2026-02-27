use anyhow::Result;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

use crate::{cache, tmutil::ExclusionManager};

/// Remove all exclusions for stale paths (those that no longer exist on disk).
///
/// Returns the number of stale paths removed.
pub fn clean_stale(
    cache_path: &Path,
    exclusion_manager: &dyn ExclusionManager,
    fixed_path: bool,
    dry_run: bool,
) -> Result<usize> {
    let mut cache = cache::load_cache(cache_path)?;
    let old_set = cache.path_set();

    let (live, stale): (Vec<PathBuf>, Vec<PathBuf>) = old_set.into_iter().partition(|p| p.exists());

    if stale.is_empty() {
        info!("clean: no stale paths found");
        return Ok(0);
    }

    for path in &stale {
        warn!("Stale path removed from exclusions: {}", path.display());
    }

    if !dry_run {
        exclusion_manager.remove_exclusions(&stale, fixed_path)?;
        cache.paths = live;
        cache::write_cache(cache_path, &cache)?;
    }

    Ok(stale.len())
}
