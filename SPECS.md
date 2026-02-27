# `letitgo` — Spec (v0.3)

> A Rust CLI tool to keep Time Machine backups lean by automatically excluding paths matched by `.gitignore` and `.lignore` files.

---

## 1. Overview

`letitgo` scans configured directories for Git repositories, resolves ignored paths via `.gitignore` (and optional `.lignore` override files), and tells Time Machine to exclude them. It can run as a one-shot command or as a periodic `launchd`/`brew services` service.

**Key design decisions:**

- **Synchronous Rust** — the `ignore` crate's built-in `crossbeam` thread pool handles parallel directory walking; `rayon` provides additional parallelism where useful
- **JSON cache** via `serde_json` — power-efficient, simple, broadest CPU compatibility
- **`.lignore` override** using Union + Negate semantics
- **Exclusion mode** (sticky vs fixed-path) is a **global** config setting
- **No async runtime** — no `tokio`. All I/O is either handled by thread pools or is trivial (single-file reads/writes, a handful of `tmutil` invocations)

---

## 2. Core Concepts

### 2.1 Ignore Sources (two layers)

| Source | Location | Purpose |
|---|---|---|
| `.gitignore` | Standard Git locations (repo root, subdirs, global) | Baseline: everything Git considers ignored is a candidate for TM exclusion |
| `.lignore` | Anywhere a `.gitignore` can be placed (repo root, subdirectories, etc.) | **Override layer** — can **add** or **un-ignore** paths relative to `.gitignore` |

#### `.lignore` Override Semantics: Union + Negate

`.lignore` uses the **same gitignore syntax**. It is applied as an overlay on top of `.gitignore`:

- **Plain lines** (e.g. `data/`) → **add** more paths to the TM exclusion set (even if not in `.gitignore`)
- **Negated lines** (e.g. `!target/release/`) → **re-include** paths that `.gitignore` would exclude, removing them from the TM exclusion set

**Example:**

```gitignore
# .gitignore          → target/ is excluded from Git (and thus from TM)
target/

# .lignore            → override for TM purposes
!target/release/      → re-include target/release/ in TM backups
data/large-dataset/   → additionally exclude from TM (may or may not be in .gitignore)
```

**Implementation:** Use the `ignore` crate's `OverrideBuilder` to layer `.lignore` rules on top of the `.gitignore` match results. This is the same mechanism ripgrep uses for `--glob` flags and supports `!` negation natively.

### 2.2 Exclusion Modes

`tmutil addexclusion` supports two modes:

| Mode | Flag | Requires sudo? | Mechanism | Persistence |
|---|---|---|---|---|
| **Sticky** (default) | _(none)_ | No | Sets `com.apple.metadata:com_apple_backup_excludeItem` xattr on the item | Follows the file/dir if moved; **lost if item is deleted & recreated** |
| **Fixed-path** | `-p` | Yes (root + Full Disk Access) | Adds path to a system-level exclusion list | Survives deletion; re-applies when a new item appears at that path |

**Configuration:** Exclusion mode is a **global config setting** (`exclusion_mode` in `config.toml`). When set to `"fixed-path"`, the periodic service plist must run with `sudo` (as a LaunchDaemon rather than LaunchAgent).

> [!IMPORTANT]
> When the user switches exclusion modes, `letitgo reset` should be run first to clear the old exclusions, since sticky and fixed-path exclusions are tracked differently by macOS.

### 2.3 State / Persistence: JSON Cache

**Format:** A JSON file at `~/Library/Caches/letitgo/cache.json`, serialized with `serde_json`.

```json
{
  "version": 1,
  "last_run": "2026-02-27T02:00:00+08:00",
  "exclusion_mode": "sticky",
  "paths": [
    "/Users/alice/project/target",
    "/Users/alice/project/node_modules"
  ]
}
```

**Why JSON:**

- **Power-efficient:** Single `read()` + single **atomic** `rename(2)` per run. No WAL/journal overhead.
- **Fast:** At this data size (~KB), `serde_json` is more than sufficient.
- **Simple:** No SQLite dependency. Easy to inspect and debug manually. Works on all architectures with zero config.
- **Deduplication:** Done in-memory with a `HashSet` before writing.
- **Crash-safe:** The write is atomic — serialised to a `NamedTempFile` in the same directory, then renamed into place. A process killed mid-write (Ctrl-C, SIGKILL, power loss) leaves the previous cache file intact; the partially-written temp file is cleaned up by the OS.

---

## 3. CLI Interface

```text
letitgo <COMMAND> [OPTIONS]

Commands:
  run       Scan, compute exclusions, and update Time Machine
  list      Show currently excluded paths (from cache)
  reset     Remove all exclusions made by letitgo and clear cache
  clean     Validate cached paths and remove stale exclusions
  init      Create a default config file with comments

Global Options:
  -c, --config <PATH>   Path to config file (default: ~/.config/letitgo/config.toml)
  -v, --verbose         Increase log verbosity (repeat for more: -vv, -vvv)
  -q, --quiet           Suppress non-error output
  --dry-run             Show what would be done without making changes
```

### 3.1 `run` subcommand

```text
letitgo run [OPTIONS]

Options:
  --search-path <DIR>   Override configured search paths (repeatable)
```

Scans search paths, computes exclusions, diffs against cache, updates Time Machine, and updates cache. **Implicitly cleans stale paths** — if a previously excluded path disappears from the scan (deleted or re-included by `.lignore`), it is automatically un-excluded.

### 3.2 `list` subcommand

```text
letitgo list [OPTIONS]

Options:
  --json                Output as JSON
  --stale               Show only paths that no longer exist on disk
```

**Default output (plain text, one path per line):**

```text
5 paths excluded from Time Machine:

  /Users/alice/projects/web-app/node_modules
  /Users/alice/projects/web-app/.parcel-cache
  /Users/alice/projects/api/target
  /Users/alice/projects/api/.cargo
  /Users/alice/Desktop/game/build
```

**`--stale` output:**

```text
2 stale paths (no longer exist on disk):

  /Users/alice/old-project/node_modules   [deleted]
  /Users/alice/tmp/build                  [deleted]
```

**Color:** Auto-detected — enabled only when stdout is a TTY, disabled when
piped or redirected. Uses `owo-colors` with `if_supports_color()`. Respects
`NO_COLOR` env var. Color scheme (TTY only):

- Count header → bold
- Normal paths → default terminal color
- `[deleted]` tag on stale paths → yellow
- Zero-result message (e.g. "No paths excluded") → dim

**`--json` output:**

```json
{
  "count": 5,
  "last_run": "2026-02-27T02:00:00+08:00",
  "exclusion_mode": "sticky",
  "paths": [
    "/Users/alice/projects/web-app/node_modules",
    "..."
  ]
}
```

### 3.3 `clean` subcommand

```
letitgo clean

  - Reads the cache
  - Checks each path still exists on disk
  - For stale paths: calls `tmutil removeexclusion` and removes from cache
  - Logs summary
```

Useful for one-off cleanup without a full re-scan.

### 3.4 `reset` subcommand

```
letitgo reset [OPTIONS]

Options:
  --yes                 Skip confirmation prompt
```

### 3.5 `init` subcommand

```
letitgo init [OPTIONS]

Options:
  --force               Overwrite existing config file
```

Creates a default `~/.config/letitgo/config.toml` with all options documented via inline comments. If the config file already exists, prints a message and exits (unless `--force` is used).

### 3.6 stdout vs stderr

| Stream | Content |
|---|---|
| **stdout** | Machine-readable data only: `list` paths (plain text), `list --json` output |
| **stderr** | All human-readable diagnostics: hints, warnings, progress, log lines (via `tracing`) |

This invariant ensures `letitgo list --json | jq .` and `letitgo list | wc -l` always
work cleanly. No diagnostic message ever leaks onto stdout.

The first-run hint is emitted as `tracing::warn!()`, which goes to stderr automatically:

```text
WARN letitgo: No config file found at ~/.config/letitgo/config.toml — using defaults.
              Run `letitgo init` to create one.
```

---

## 4. Configuration

**Location:** `~/.config/letitgo/config.toml`

On first run of any command (except `init`), if no config file exists, `letitgo` runs
with sensible defaults and emits a hint **to stderr** via `tracing::warn!()`:

```toml
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

# Glob patterns for paths to always include in backups (whitelist)
whitelist = [
    "**/application.yml",
    "**/.env",
    "**/.env.*",
]

# Exclusion mode: "sticky" (default) or "fixed-path"
# "fixed-path" requires running with sudo
exclusion_mode = "sticky"
```

---

## 5. Architecture & Key Crates

### 5.1 Execution Model: Synchronous + Thread Parallelism

**No async runtime.** All parallelism is thread-based:

| Operation | I/O characteristics | Parallelism approach |
|---|---|---|
| Directory traversal (find `.git` dirs) | I/O + CPU bound | `ignore::WalkBuilder::build_parallel()` — `crossbeam` thread pool |
| Per-repo ignore resolution | I/O (small files) + CPU | `rayon::par_iter()` over discovered repos |
| Reading/writing cache | I/O (single file) | Sequential — trivial, microseconds |
| Reading config | I/O (single file) | Sequential — trivial |
| Spawning `tmutil` processes | I/O (process spawn) | Sequential or `rayon` batched — only ~5-10 invocations |

### 5.2 How the `ignore` Crate Parallelizes Walking

The `ignore` crate (by BurntSushi, the ripgrep author) provides `WalkBuilder::build_parallel()`:

1. Spawns a thread pool (defaults to number of logical CPUs)
2. Work-stealing: each thread takes a directory off the queue, reads entries, pushes subdirectories back
3. `.gitignore` rules are parsed and applied **during the walk**, so ignored subtrees are pruned early (never traversed)
4. Uses `crossbeam` internally for the work-stealing deque

### 5.3 Recommended Crates

| Crate | Purpose |
|---|---|
| `ignore` | `.gitignore` parsing + parallel directory walking (by BurntSushi) |
| `rayon` | Data-parallel iteration for per-repo processing and tmutil batching |
| `clap` (derive) | CLI argument parsing |
| `serde` + `toml` | Config file parsing |
| `serde_json` | JSON cache serialization |
| `tracing` + `tracing-subscriber` | Structured logging |
| `anyhow` | Error handling |
| `directories` | XDG/macOS standard paths (`~/Library/Caches`, etc.) |
| `chrono` | Timestamps in cache |
| `walkdir` | Raw directory traversal for the two-pass ignore resolution |
| `fd-lock` | Advisory file locking for `/tmp/letitgo.lock` |
| `owo-colors` | TTY-aware terminal colors (auto-disables when piped) |

### 5.4 Module Structure

```
src/
├── main.rs           # Entry point, CLI dispatch
├── cli.rs            # Clap command/arg definitions
├── config.rs         # TOML config file parsing
├── scanner.rs        # Repo discovery (parallel walk, find .git dirs)
├── ignore_resolver.rs # .gitignore + .lignore resolution, override logic
├── tmutil.rs         # tmutil command wrapper (add/remove exclusion)
├── cache.rs          # JSON cache read/write/diff
├── clean.rs          # Path validation & stale cleanup
└── error.rs          # Error types
```

---

## 6. Core Algorithm (`run`)

### 6.1 High-Level Flow

```
 1. Load config (TOML)
 2. Load cache (JSON via serde_json — previous exclusion set as HashSet)
 3. Acquire lockfile (/tmp/letitgo.lock) — skip run if already held
 4. Scan search_paths for Git repos:
    - Use ignore::WalkBuilder with parallel walking
    - Filter for .git directories
    - Skip ignored_paths from config
    → Produces Vec<PathBuf> of repo roots
 5. For each repo (in parallel via rayon), resolve excluded paths:
    → Two-pass algorithm (see §6.2 below)
 6. Merge all repos → deduplicated HashSet<PathBuf>
 7. Diff against cache:
    - paths_to_add    = new_set - cached_set
    - paths_to_remove = cached_set - new_set
 8. Batch tmutil calls — adds and removes are separate verbs but both accept
    multiple paths per invocation. Chunk each set at ~200 paths/call to stay
    well under macOS ARG_MAX (≈256 KB). The two sets are independent and can
    run in parallel via rayon:
    - paths_to_add in chunks    → tmutil addexclusion    [-p] path1 path2 ... path200
    - paths_to_remove in chunks → tmutil removeexclusion [-p] path1 path2 ... path200
    In practice the diff is usually small (a handful of paths), so most runs
    produce only 1–2 tmutil invocations total. The initial run on a fresh machine
    may have thousands of paths, making chunking important.
 9. Write cache atomically: serialise new_set + timestamp + exclusion_mode to a
    sibling NamedTempFile, then rename(2) it into place. The advisory lock is
    held during this step. If the process is killed at any point, the previous
    cache file remains intact (rename is atomic on POSIX; the temp file is
    reclaimed by the OS).
10. Release lockfile (or: lock is released automatically by the OS when the
    process exits — flock(2) is per-open-file-description, not per-process).
11. Log summary (# added, # removed, # total, duration)
```

### 6.2 Per-Repo Ignore Resolution (Two-Pass Algorithm)

Extracting ignored paths from the `ignore` crate requires care, because
`WalkBuilder` emits **non-ignored** entries and prunes ignored subtrees.
We use `GitignoreBuilder` + `walkdir` instead, with a two-pass approach.

```
Pass 1 — Collect ignored directories (with pruning):

  1. Build a Gitignore matcher from all .gitignore files in the repo
     (root + subdirectories) using ignore::gitignore::GitignoreBuilder.
  2. Walk the repo with walkdir::WalkDir.
  3. For each entry, call gitignore.matched(path, is_dir):
     - Match::Ignore + is_dir → add to excluded_dirs, call skip_current_dir()
       (prune: don't descend into ignored directories)
     - Match::Ignore + is_file → add to excluded_files (optional, see note)
     - Match::None / Match::Whitelist → continue walking

Pass 2 — Apply .lignore overrides (exact-match negation only):

  4. Load .lignore files into a second GitignoreBuilder, with each .lignore
     file rooted at the same directory as its co-located .gitignore — mirroring
     standard gitignore path scoping. A .lignore at repo-root/ has global scope
     (can reference paths produced by any subdirectory .gitignore); a .lignore
     at src/ is scoped to paths under src/.
  5. For plain (non-negated) lines in .lignore:
     - Add matching paths to the exclusion set (additional exclusions).
  6. For negation patterns (lines starting with `!`):
     a. Resolve the negated pattern to an absolute path.
     b. If that absolute path is a DIRECT ENTRY in the exclusion set, remove
        it. E.g. `!target/` in repo-root/.lignore removes `repo-root/target/`
        from the set. This works regardless of which .gitignore file (root-
        level or subdirectory) originally caused the path to be excluded.
     c. If the resolved path is a SUB-PATH of an excluded directory entry
        (i.e., an entry in the exclusion set is a prefix of the negated path —
        e.g., `!target/release/` while `target/` is in the set), emit a
        runtime warning and skip:

        WARN: .lignore negation `!target/release/` targets a sub-path of
              excluded directory `target/`. Sub-path negation is not yet
              supported — `target/` remains fully excluded from backups.
              Workaround: use `!target/` to fully un-exclude the directory.

  7. Apply whitelist from config: remove paths matching whitelist globs.
```

> [!NOTE]
> **"Exact-match negation" clarified:** The restriction is about _pattern depth
> in the exclusion set_, not about _file co-location_. A root-level `.lignore`
> with `!src/vendor/` **can** remove `src/vendor/` from the exclusion set even
> if it was originally excluded by `src/.gitignore` — because the exclusion set
> is a flat `HashSet` of resolved absolute paths and the match is by equality.
> The limitation only applies when the negated path is _inside_ a directory that
> was pruned whole (and therefore its children were never individually recorded).

<!-- -->

> [!NOTE]
> **Directory-level vs file-level exclusion:** For efficiency, we primarily
> track **directory-level** exclusions. Excluding `node_modules/` with one
> `tmutil` call is far better than excluding thousands of files inside it.
> Individual files are only tracked when they match a file-level gitignore
> pattern (e.g., `*.log`) and are not inside an already-excluded directory.

> [!NOTE]
> **Sub-path negation** (e.g., `!target/release/` when `target/` is excluded)
> is deferred to a future version. Implementing it correctly requires
> "exploding" the parent exclusion into per-child exclusions, which adds
> significant complexity. The runtime warning ensures users are aware of
> the limitation.

---

## 7. Periodic Service (`brew services`)

### 7.1 Sticky mode — LaunchAgent (no sudo)

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.github.ifsheldon.letitgo</string>
    <key>ProgramArguments</key>
    <array>
        <string>HOMEBREW_PREFIX/bin/letitgo</string>
        <string>run</string>
    </array>
    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>2</integer>
        <key>Minute</key>
        <integer>0</integer>
    </dict>
    <key>StandardOutPath</key>
    <string>/tmp/letitgo-out.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/letitgo-err.log</string>
</dict>
</plist>
```

### 7.2 Fixed-path mode — LaunchDaemon (requires sudo)

When `exclusion_mode = "fixed-path"`, the plist would need to be installed as a LaunchDaemon (`/Library/LaunchDaemons/`) instead of a LaunchAgent, so it runs as root.

> [!WARNING]
> `brew services` installs LaunchAgents by default (user-level). For LaunchDaemons (root-level), `sudo brew services start letitgo` is needed. The Homebrew formula should document this distinction.

### 7.3 Homebrew Formula (skeleton)

```ruby
class Letitgo < Formula
  desc "Exclude gitignored files from Time Machine backups"
  homepage "https://github.com/ifsheldon/LetItGo"
  url "https://github.com/ifsheldon/LetItGo/archive/refs/tags/vX.Y.Z.tar.gz"
  sha256 "..."
  license "Apache-2.0"

  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
  end

  service do
    run [opt_bin/"letitgo", "run"]
    run_type :cron
    cron "0 2 * * *"
    log_path var/"log/letitgo.log"
    error_log_path var/"log/letitgo.log"
  end

  test do
    assert_match "letitgo", shell_output("#{bin}/letitgo --version")
  end
end
```

---

## 8. Scope: v1 vs Future

| Feature | v1 | Future |
|---|---|---|
| `.gitignore` parsing + TM exclusion | ✅ | |
| `.lignore` support (Union + Negate) | ✅ | |
| Sticky exclusion mode | ✅ | |
| Fixed-path exclusion mode (`-p`) | ✅ | |
| JSON cache + diff (`serde_json`) | ✅ | |
| `run`, `list`, `reset`, `clean` commands | ✅ | |
| `--dry-run` | ✅ | |
| Homebrew formula + `brew services` | ✅ | |
| Config file (TOML) | ✅ | |
| iCloud Drive exclusion (`com.apple.fileprovider.ignore#P` xattr) | | TODO |
| `status` command (rich summary dashboard) | | TODO |
| Real-time file-system watcher (via `notify` / FSEvents) | | TODO |

### TODO: Real-time File-System Watcher (Future)

> [!NOTE]
> **Concept:** Instead of a periodic cron job, run `letitgo` as a persistent daemon that watches code directories via macOS FSEvents (using the [`notify`](https://docs.rs/notify) crate). When `.gitignore` or `.lignore` files change, or when new directories matching ignored patterns appear, TM exclusions are updated immediately.
>
> **Considerations:**
>
> - Requires a long-running daemon (LaunchAgent/Daemon), not a periodic job
> - Needs event debouncing (filesystem events fire rapidly during builds)
> - Higher baseline memory/power usage vs periodic job (keeps file watchers open)
> - Different execution model — may warrant a separate `letitgo watch` subcommand
> - FSEvents on macOS is efficient (kernel-level, coalesced), so power impact is moderate
>
> **When to implement:** After v1 is stable and real-world usage confirms that periodic scanning is insufficient (e.g., users want instant exclusion of `node_modules` after `npm install`).

---

## 9. Edge Cases to Handle

1. **Nested Git repos** (submodules) — each should be scanned independently
2. **Symlinks** — do NOT follow them (avoid infinite loops)
3. **Very large repos** — e.g. monorepos with thousands of ignored paths. Batch `tmutil` calls (200 paths per invocation)
4. **Permission errors** — some dirs may not be readable. Log warning and skip
5. **Concurrent runs** — protected by `/tmp/letitgo.lock` advisory file lock. If a second instance can't acquire the lock, it logs a warning and exits gracefully.
6. **Signal safety (Ctrl-C / SIGKILL)** — `flock(2)` advisory locks are per-open-file-description; the OS releases them automatically when the process exits, regardless of how it is killed (even SIGKILL, even without Rust `Drop` running). Cache writes are atomic (temp-file + `rename(2)`), so a killed process leaves no corrupt state — the previous cache file remains intact.
7. **`tmutil` failures** — handle non-zero exit codes gracefully (e.g. exit code 213 = path not found, safe to ignore)
8. **Mode switching** — if user switches from sticky to fixed-path (or vice versa), warn that `reset` should be run first to clean up the old exclusion type. Record the mode in the cache file for detection
9. **Empty `.lignore`** — if present but empty, it has no effect (neither adds nor negates)
10. **Global `.gitignore`** — the `ignore` crate respects `core.excludesfile` from Git config automatically

---

## 10. Testing Strategy

### Design Constraint

This tool runs on a daily-use Mac. Tests must **never** leave permanent files,
configs, xattrs, or system-level changes. Everything must be isolated and
automatically cleaned up.

### 10.1 Dependency Injection for `tmutil`

The core architectural decision for testability: abstract `tmutil` behind a trait.
Production code calls the real binary; tests use a mock that records calls.

```rust
// Send + Sync required: AppContext (which owns Box<dyn ExclusionManager>) may be
// referenced from rayon scopes even though ExclusionManager itself is only *called*
// after rayon's parallel section completes. Without Send + Sync, the borrow checker
// will reject any code that passes a reference to AppContext into a rayon::scope.
pub trait ExclusionManager: Send + Sync {
    fn add_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()>;
    fn remove_exclusions(&self, paths: &[PathBuf], fixed_path: bool) -> Result<()>;
    fn is_excluded(&self, path: &Path) -> Result<bool>;
}

/// Production: stateless, trivially Send + Sync — calls /usr/bin/tmutil
pub struct TmutilManager;

/// Tests: records calls in-memory, never touches the system.
/// Uses Mutex (not RefCell) to satisfy Send + Sync.
/// Lock contention is negligible — only a handful of calls per test.
pub struct MockExclusionManager {
    pub added: Mutex<Vec<PathBuf>>,
    pub removed: Mutex<Vec<PathBuf>>,
}
```

### 10.2 Configurable Paths via `AppContext`

All file system paths are injectable through a context struct, so tests can
redirect everything to a temp directory:

```rust
pub struct AppContext {
    pub config_path: PathBuf,              // default: ~/.config/letitgo/config.toml
    pub cache_path: PathBuf,               // default: ~/Library/Caches/letitgo/cache.json
    pub lock_path: PathBuf,                // default: /tmp/letitgo.lock
    pub exclusion_manager: Box<dyn ExclusionManager>,
}
```

In tests:

```rust
#[test]
fn test_run_excludes_gitignored_dirs() {
    let tmp = tempfile::tempdir().unwrap();  // auto-deleted on Drop
    let mock = MockExclusionManager::new();
    let ctx = AppContext {
        config_path: tmp.path().join("config.toml"),
        cache_path: tmp.path().join("cache.json"),
        lock_path: tmp.path().join("letitgo.lock"),
        exclusion_manager: Box::new(mock),
    };
    // ... create fake repo with .gitignore in tmp ...
    // ... run the algorithm ...
    // ... assert mock.added contains expected paths ...
}   // tmp dir + all contents deleted here automatically
```

### 10.3 Test Levels

| Level | What it tests | System impact | Runs in CI? |
|---|---|---|---|
| **Unit tests** | Config parsing, cache read/write/diff, ignore resolution, CLI args | **None** — temp dirs only | ✅ |
| **Integration tests** | Full `run`/`clean`/`reset`/`list` flow with mock tmutil | **None** — mock + temp dirs | ✅ |
| **Smoke tests** (manual) | Actually calls `tmutil` on temp files, verifies xattr, cleans up | **Temporary** — cleanup in test | Manual only |

### 10.4 Test Fixtures: Fake Git Repos

Create throwaway repos in `tempdir()` with `.gitignore` and `.lignore` files:

```rust
/// Creates a fake Git repo structure in a temp directory
fn create_test_repo(root: &Path) -> PathBuf {
    let repo = root.join("test-repo");
    fs::create_dir_all(repo.join(".git")).unwrap();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(repo.join("target/debug")).unwrap();
    fs::create_dir_all(repo.join("target/release")).unwrap();
    fs::create_dir_all(repo.join("node_modules/foo")).unwrap();
    fs::write(repo.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
    repo
}

/// Creates a repo with .lignore override
fn create_test_repo_with_lignore(root: &Path) -> PathBuf {
    let repo = create_test_repo(root);
    fs::write(repo.join(".lignore"), "!target/\ndata/\n").unwrap();
    fs::create_dir_all(repo.join("data")).unwrap();
    repo
}
```

### 10.5 Key Test Scenarios

**Unit tests:**

- Config: parse valid TOML, missing fields use defaults, invalid TOML returns error, `expand_tilde`, `is_fixed_path`, resolved search paths
- Cache: round-trip read/write (atomic via rename), diff computation (added/removed/identical sets), empty cache, nested parent-dir creation
- Error: `is_tmutil_safe_error` distinguishes code 213 from all others
- Scanner: basic repo discovery, ignored-path filtering, nested submodules, multiple search paths, symlinks not followed, missing search path
- Ignore resolution: `.gitignore` directory-level and file-level patterns, nested `.gitignore` and `.lignore` scoped to subdirectory, `.lignore` additions/negations, whitelist filtering, sub-path negation warning, repo with no `.gitignore`, symlinks not followed inside repo, `.git` dir not in exclusion set

**Integration tests** (all use `MockExclusionManager` + temp dirs; zero system impact):

- `run`: fresh repo — `add_exclusions` called with correct paths, cache written
- `run`: incremental — only diff sent (new paths added, removed paths un-excluded), verified via both cache and mock call inspection
- `run`: `.lignore` negation — negated paths absent from mock's `add_exclusions` calls
- `run`: `--dry-run` — cache not written, `ExclusionManager` never called
- `run`: `--search-path` override supersedes config paths
- `run`: mode-switch (sticky → fixed-path) — emits warning, does not crash
- `run`: repo with no `.gitignore` — no exclusions added
- `run`: config whitelist — whitelisted paths excluded from `add_exclusions` even when in `.gitignore`
- `list`: empty cache, with live + stale paths, `--json`, `--stale` (all variants — no crash, correct output)
- `clean`: stale paths removed from cache and un-excluded; `--dry-run` leaves cache unchanged
- `reset`: cache deleted and exclusions removed; empty cache handled gracefully; `--dry-run` leaves cache unchanged and skips `remove_exclusions`
- `init`: creates config with nested dirs, `--force` overwrites, no-overwrite by default
- Lockfile: concurrent run attempt (lock manually held) exits gracefully without writing cache

**Smoke tests (manual, opt-in):**

- Create temp dir, call real `tmutil addexclusion`, verify xattr is set, call `tmutil removeexclusion`, verify xattr is removed

### 10.6 Test Crates

| Crate | Purpose |
|---|---|
| `tempfile` | Creates temp dirs/files that auto-delete on `Drop` |
| `assert_cmd` | CLI integration tests — run the compiled binary, check stdout/stderr/exit code |
| `predicates` | Fluent assertion helpers for `assert_cmd` output matching |
