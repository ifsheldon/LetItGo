# LetItGo

**Keep Time Machine backups lean by automatically excluding gitignored paths.**

[![CI](https://github.com/ifsheldon/LetItGo/actions/workflows/ci.yml/badge.svg)](https://github.com/ifsheldon/LetItGo/actions/workflows/ci.yml)
[![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg)](https://opensource.org/licenses/Apache-2.0)
![Platform: macOS](https://img.shields.io/badge/platform-macOS-lightgrey)

---

## Why?

Time Machine doesn't know about `.gitignore`. It faithfully backs up everything —
including the multi-gigabyte directories that your build tools recreate from scratch
on every clean build:

```
~/projects/api/target/              ← Rust build artifacts  (often 5–20 GB)
~/projects/web/node_modules/        ← npm packages          (often 1–3 GB)
~/projects/ml/.venv/                ← Python virtualenv      (hundreds of MB)
~/projects/ios/Pods/                ← CocoaPods              (hundreds of MB)
```

These directories:

- **change constantly**, triggering frequent incremental backups
- **are never unique** — you can always recreate them with `cargo build` / `npm install` / etc.
- **inflate backup size and duration** for no benefit

`letitgo` reads your `.gitignore` files and tells Time Machine to skip the right paths —
automatically, incrementally, on a schedule.

---

## How It Works

```
letitgo run
    │
    ├─ Scan search_paths for Git repos  (parallel, ignore::WalkBuilder)
    │
    ├─ For each repo: resolve excluded paths  (rayon parallel)
    │       ├─ Single-pass walk: apply .gitignore rules incrementally
    │       │     (discovers .gitignore files during traversal, prunes ignored dirs)
    │       ├─ Apply .lignore overrides (add or un-exclude paths)
    │       └─ Apply whitelist (never exclude .env, application.yml, …)
    │
    ├─ Diff against cache  (HashSet subtract)
    │       ├─ new paths → tmutil addexclusion path1 path2 …
    │       └─ removed paths → tmutil removeexclusion path1 path2 …
    │
    └─ Write cache atomically  (NamedTempFile + rename(2))
```

Subsequent runs are **incremental** — only the diff is sent to `tmutil`,
so a steady-state run that finds nothing new takes milliseconds.

---

## Installation

### From source

```sh
cargo install --git https://github.com/ifsheldon/LetItGo
```

### Build locally

```sh
git clone https://github.com/ifsheldon/LetItGo
cd LetItGo
cargo build --release
# binary is at ./target/release/letitgo
```

> **Requirements:** Rust 1.85+ (edition 2024), macOS 12+

---

## Quick Start

```sh
# 1. Create a config file with sensible defaults
letitgo init

# 2. Do a dry run to see what would be excluded (no changes made)
letitgo --dry-run run

# 3. Apply exclusions
letitgo run

# 4. See what's excluded
letitgo list
```

That's it. Schedule `letitgo run` as a daily job to keep exclusions up to date
as your projects evolve (see [Running as a Service](#running-as-a-service)).

---

## Commands

### `letitgo run`

Scan repos, compute exclusions, and update Time Machine.

```sh
letitgo run [--search-path <DIR>]...
```

- Discovers all Git repos under `search_paths` (from config)
- Resolves gitignored paths via `.gitignore` and `.lignore`
- **Diffs** the result against the cache — only changed paths call `tmutil`
- Writes the updated cache atomically

Override the configured search paths without editing the config:

```sh
letitgo run --search-path ~/projects/work --search-path ~/projects/personal
```

Use `--dry-run` to preview changes without touching Time Machine or the cache:

```sh
letitgo --dry-run run
```

---

### `letitgo list`

Show currently excluded paths (read from cache — no scanning).

```sh
letitgo list [--json] [--stale]
```

```
5 paths excluded from Time Machine:

  /Users/alice/projects/web-app/node_modules
  /Users/alice/projects/web-app/.parcel-cache
  /Users/alice/projects/api/target
  /Users/alice/Desktop/game/build
```

| Flag | Effect |
|------|--------|
| `--json` | Machine-readable JSON output (safe to pipe to `jq`) |
| `--stale` | Show only paths that no longer exist on disk |

```sh
# Count excluded paths
letitgo list --json | jq '.count'

# Find exclusions pointing to deleted directories
letitgo list --stale
```

---

### `letitgo clean`

Remove exclusions for paths that no longer exist on disk.

```sh
letitgo clean
```

Useful after deleting old projects — frees up the Time Machine exclusion entries
without doing a full re-scan. Use `--dry-run` to preview:

```sh
letitgo --dry-run clean
```

---

### `letitgo reset`

Remove **all** exclusions created by `letitgo` and clear the cache.

```sh
letitgo reset [--yes]
```

Prompts for confirmation unless `--yes` is passed. Use this before switching
exclusion modes (sticky ↔ fixed-path) or when uninstalling `letitgo`.

```sh
letitgo reset --yes
```

---

### `letitgo init`

Create a default config file at `~/.config/letitgo/config.toml`.

```sh
letitgo init [--force]
```

Does nothing if the config already exists. Use `--force` to overwrite.

---

### Global flags

| Flag | Effect |
|------|--------|
| `-c, --config <PATH>` | Use a different config file |
| `--dry-run` | Preview changes — no `tmutil` calls, no cache writes |
| `-v / -vv` | Increase log verbosity (`-v` = DEBUG, `-vv` = TRACE) |
| `-q, --quiet` | Suppress all output except errors |

Logs go to **stderr**; `list` output goes to **stdout** — piping always works cleanly.

---

## Configuration

`letitgo init` creates `~/.config/letitgo/config.toml` with all options explained:

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

# Glob patterns for paths to always include in backups.
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
```

### Whitelist

The whitelist prevents `letitgo` from excluding paths you want backed up
even if they appear in `.gitignore`. The defaults protect common secrets files:

```toml
whitelist = [
    "**/application.yml",   # Spring Boot secrets
    "**/.env",              # dotenv files
    "**/.env.*",            # .env.local, .env.production, …
]
```

Globs are matched against absolute paths using [`globset`](https://docs.rs/globset) syntax.

---

## `.lignore` Override Files

Place a `.lignore` file anywhere you'd place a `.gitignore`. It uses the same
syntax and acts as an **override layer** on top of `.gitignore`:

| Line type | Effect |
|-----------|--------|
| `data/` | **Add** `data/` to the Time Machine exclusion set (even if not in `.gitignore`) |
| `!target/release/` | **Remove** `target/release/` from the exclusion set (re-include in backups) |

**Example:** You want to back up release builds but not debug builds:

```gitignore
# .gitignore  — target/ is gitignored (and would be TM-excluded by default)
target/
```

```gitignore
# .lignore  — override for Time Machine purposes
!target/          # re-include everything in target/ in TM backups
target/debug/     # then additionally exclude just the debug build
```

> **Note:** Sub-path negation (`!target/release/` when `target/` is the excluded
> entry) is not yet supported. `letitgo` will emit a warning and keep the parent
> directory excluded. Use `!target/` to un-exclude the whole directory, then add
> specific sub-paths you want to exclude.

---

## Exclusion Modes

| Mode | Config value | `sudo`? | Mechanism | Lost when item is deleted? |
|------|-------------|---------|-----------|---------------------------|
| **Sticky** *(default)* | `"sticky"` | No | Sets `com.apple.metadata:com_apple_backup_excludeItem` xattr | Yes — must re-run `letitgo run` |
| **Fixed-path** | `"fixed-path"` | Yes | Adds path to `/Library/Preferences/com.apple.TimeMachine.plist` | No — re-applies automatically |

**Sticky** is the right choice for most users — no `sudo` needed, and `letitgo run`
on a schedule recreates any lost exclusions after a clean build.

**Fixed-path** is useful when you want the exclusion to persist even when a build tool
deletes and recreates a directory (e.g. `cargo clean` removes `target/` entirely).
It requires running `letitgo` as root.

> **Important:** Run `letitgo reset` before switching modes to remove exclusions set
> with the old mechanism. Sticky xattrs and fixed-path plist entries are tracked
> separately by macOS and won't interfere with each other, but `letitgo`'s cache
> tracks them as a single set.

---

## Running as a Service

Schedule `letitgo run` to keep exclusions current as your projects evolve.

### LaunchAgent (sticky mode — no sudo)

Save the following plist to `~/Library/LaunchAgents/com.github.ifsheldon.letitgo.plist`:

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
        <string>/usr/local/bin/letitgo</string>
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

Load it:

```sh
launchctl load ~/Library/LaunchAgents/com.github.ifsheldon.letitgo.plist
```

This runs `letitgo run` every day at 2:00 AM. Adjust `Hour` and `Minute` to taste.

### LaunchDaemon (fixed-path mode — requires sudo)

For `exclusion_mode = "fixed-path"`, install the plist as a LaunchDaemon
(`/Library/LaunchDaemons/`) and load it with `sudo launchctl load …` so it
runs as root.

---

## Building & Testing

```sh
# Build
cargo build

# Run all tests (67 tests, all using mock tmutil — zero system impact)
cargo test

# Lint
cargo clippy --all-features
cargo fmt --check
```

The test suite uses a `MockExclusionManager` for all tests — no real `tmutil` calls
are made, no xattrs are set, and all file I/O is isolated to temp directories that
are deleted automatically on test completion.

The [CI workflow](.github/workflows/ci.yml) runs everything on `macos-latest` (free for public repos):

- **Lint**: `cargo fmt --check` + `cargo clippy`
- **Unit + integration tests**: all 67 tests with mock (zero system impact)
- **Real tmutil smoke test**: `addexclusion` → `isexcluded` → `removeexclusion` against actual `tmutil`

---

## License

Apache License 2.0 — see [LICENSE](LICENSE) or <https://opensource.org/licenses/Apache-2.0>.
