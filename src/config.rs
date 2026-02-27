use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The default config file contents, written by `letitgo init`.
pub const DEFAULT_CONFIG: &str = r#"# letitgo configuration
# Location: ~/.config/letitgo/config.toml

# Directories to scan for Git repos
search_paths = ["~"]

# Directories to skip during scan
ignored_paths = [
    "~/.Trash",
    "~/Applications",
    "~/Downloads",
    "~/Library",
    "~/Music",
    "~/Pictures",
]

# Glob patterns for paths to always include in backups (whitelist).
# Paths matching these globs will NOT be excluded from Time Machine,
# even if they are matched by .gitignore.
whitelist = [
    "**/application.yml",
    "**/.env",
    "**/.env.*",
]

# Exclusion mode: "sticky" (default) or "fixed-path"
#
# sticky:     Sets an extended attribute on each item. No sudo needed.
#             Exclusion follows the file if moved, but is lost if the
#             item is deleted and recreated.
#
# fixed-path: Registers paths in a system-level exclusion list. Requires
#             sudo (run as root). Survives deletion; re-applies when a
#             new item appears at the same path.
#
# IMPORTANT: run `letitgo reset` before switching modes.
exclusion_mode = "sticky"
"#;

/// How Time Machine exclusions are applied to the filesystem.
///
/// The two modes differ in where the exclusion metadata is stored and whether
/// `sudo` is required.  Run `letitgo reset` before switching modes to clear
/// exclusions set with the previous method.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExclusionMode {
    /// Sets an extended attribute (`com.apple.metadata:com_apple_backup_excludeItem`)
    /// directly on each item.  No `sudo` required.  The exclusion travels with
    /// the item if it is moved, but is lost if the item is deleted and recreated
    /// (e.g. a clean `cargo build` removes and recreates `target/`).
    #[default]
    Sticky,

    /// Registers paths in `/Library/Preferences/com.apple.TimeMachine.plist`
    /// via `tmutil addexclusion -p`.  Requires `sudo`.  Survives item deletion
    /// and re-creation at the same path, but does not follow moves.
    FixedPath,
}

impl ExclusionMode {
    /// Returns `true` when this is [`ExclusionMode::FixedPath`].
    ///
    /// Used internally to decide whether to pass the `-p` flag to `tmutil`.
    pub fn is_fixed_path(&self) -> bool {
        matches!(self, ExclusionMode::FixedPath)
    }
}

impl std::fmt::Display for ExclusionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExclusionMode::Sticky => write!(f, "sticky"),
            ExclusionMode::FixedPath => write!(f, "fixed-path"),
        }
    }
}

/// Runtime configuration loaded from `~/.config/letitgo/config.toml`.
///
/// All fields have compile-time defaults that match [`DEFAULT_CONFIG`], so the
/// tool works without a config file (though `letitgo init` is recommended).
/// Tilde paths in `search_paths` and `ignored_paths` are expanded at runtime
/// via [`Config::resolved_search_paths`] / [`Config::resolved_ignored_paths`].
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Directories to scan for Git repositories (supports `~`).
    #[serde(default = "default_search_paths")]
    pub search_paths: Vec<String>,

    /// Directories to skip entirely during the repo scan (supports `~`).
    #[serde(default = "default_ignored_paths")]
    pub ignored_paths: Vec<String>,

    /// Glob patterns for paths that should never be excluded from Time Machine,
    /// even if they are matched by `.gitignore` (e.g. `**/.env`).
    #[serde(default = "default_whitelist")]
    pub whitelist: Vec<String>,

    /// How to register exclusions with Time Machine — see [`ExclusionMode`].
    #[serde(default)]
    pub exclusion_mode: ExclusionMode,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            search_paths: default_search_paths(),
            ignored_paths: default_ignored_paths(),
            whitelist: default_whitelist(),
            exclusion_mode: ExclusionMode::Sticky,
        }
    }
}

fn default_search_paths() -> Vec<String> {
    vec!["~".to_string()]
}

fn default_ignored_paths() -> Vec<String> {
    vec![
        "~/.Trash".to_string(),
        "~/Applications".to_string(),
        "~/Downloads".to_string(),
        "~/Library".to_string(),
        "~/Music".to_string(),
        "~/Pictures".to_string(),
    ]
}

fn default_whitelist() -> Vec<String> {
    vec![
        "**/application.yml".to_string(),
        "**/.env".to_string(),
        "**/.env.*".to_string(),
    ]
}

impl Config {
    /// Load config from `path`. Returns `(Config, found)` — if the file does
    /// not exist, returns the default config and `found = false`.
    pub fn load(path: &Path) -> Result<(Self, bool)> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let config: Config = toml::from_str(&text)
                    .with_context(|| format!("parsing config file: {}", path.display()))?;
                Ok((config, true))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((Config::default(), false)),
            Err(e) => {
                Err(e).with_context(|| format!("reading config file: {}", path.display()))
            }
        }
    }

    /// Expand `~` in every entry of `search_paths` and return absolute `PathBuf`s.
    pub fn resolved_search_paths(&self) -> Vec<PathBuf> {
        self.search_paths.iter().map(|p| expand_tilde(p)).collect()
    }

    /// Expand `~` in every entry of `ignored_paths` and return absolute `PathBuf`s.
    pub fn resolved_ignored_paths(&self) -> Vec<PathBuf> {
        self.ignored_paths.iter().map(|p| expand_tilde(p)).collect()
    }
}

/// Expand a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    } else if path == "~"
        && let Some(home) = dirs_home()
    {
        return home;
    }
    PathBuf::from(path)
}

fn dirs_home() -> Option<PathBuf> {
    directories::UserDirs::new().map(|u| u.home_dir().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_default_config() {
        let cfg = Config::default();
        assert_eq!(cfg.exclusion_mode, ExclusionMode::Sticky);
        assert!(!cfg.search_paths.is_empty());
    }

    #[test]
    fn test_parse_valid_toml() {
        let toml = r#"
            search_paths = ["/tmp/repos"]
            ignored_paths = ["/tmp/skip"]
            whitelist = ["**/.env"]
            exclusion_mode = "sticky"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.search_paths, vec!["/tmp/repos"]);
        assert_eq!(cfg.exclusion_mode, ExclusionMode::Sticky);
    }

    #[test]
    fn test_parse_fixed_path_mode() {
        let toml = r#"exclusion_mode = "fixed-path""#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.exclusion_mode, ExclusionMode::FixedPath);
    }

    #[test]
    fn test_missing_fields_use_defaults() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.exclusion_mode, ExclusionMode::Sticky);
        assert!(!cfg.search_paths.is_empty());
    }

    #[test]
    fn test_invalid_toml_returns_error() {
        let result: Result<Config, _> = toml::from_str("not = [valid toml");
        assert!(result.is_err());
    }

    #[test]
    fn test_load_missing_file_returns_default() {
        let (cfg, found) = Config::load(Path::new("/nonexistent/path/config.toml")).unwrap();
        assert!(!found);
        assert_eq!(cfg.exclusion_mode, ExclusionMode::Sticky);
    }

    #[test]
    fn test_load_existing_file() {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        write!(f, "search_paths = [\"/tmp\"]\n").unwrap();
        let (cfg, found) = Config::load(f.path()).unwrap();
        assert!(found);
        assert_eq!(cfg.search_paths, vec!["/tmp"]);
    }

    #[test]
    fn test_expand_tilde() {
        // Non-tilde paths pass through unchanged
        assert_eq!(
            expand_tilde("/absolute/path"),
            PathBuf::from("/absolute/path")
        );
        assert_eq!(expand_tilde("relative"), PathBuf::from("relative"));

        // Tilde expansion requires a discoverable home dir
        if let Some(home) = dirs_home() {
            assert_eq!(expand_tilde("~"), home);
            assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        }
    }

    #[test]
    fn test_exclusion_mode_is_fixed_path() {
        assert!(!ExclusionMode::Sticky.is_fixed_path());
        assert!(ExclusionMode::FixedPath.is_fixed_path());
    }

    #[test]
    fn test_resolved_search_paths_expands_tilde() {
        if let Some(home) = dirs_home() {
            let cfg = Config {
                search_paths: vec!["~".to_string(), "/absolute".to_string()],
                ..Config::default()
            };
            let resolved = cfg.resolved_search_paths();
            assert_eq!(resolved[0], home);
            assert_eq!(resolved[1], PathBuf::from("/absolute"));
        }
    }
}
