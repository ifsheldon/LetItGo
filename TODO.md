# TODO: Exclude build caches from backups

## Existing tools (reference)
- **tmignore** — parses `.gitignore` and excludes matched items from TM. Homebrew installable.
  No longer maintained but proves the concept.
- **asimov** — simpler approach, scans for known dependency dirs (hardcoded list, no `.gitignore` parsing).
- **ignore** crate (Rust, by ripgrep author) — robust `.gitignore` parser, no TM integration.

## Plan: Build a Rust CLI tool
- Parse `.gitignore` files to find ignored directories and exclude from Time Machine
- Use the `ignore` crate for `.gitignore` parsing
- Exclude via `tmutil addexclusion` (no `-p`, no sudo needed; sets xattr on directory)
- Note: xattr exclusion is lost if directory is deleted and recreated
- Run as a scheduled launchd job (midnight–3am window)

## iCloud Drive considerations
- iCloud Desktop & Documents sync is enabled (`optimize-storage = 1`)
- iCloud does **not** respect `.gitignore` — it syncs everything including `node_modules`
- Exclusion methods:
  - `.nosync` extension: rename folder (e.g. `node_modules.nosync`) — hacky, breaks tooling
  - `xattr` method: set `com.apple.fileprovider.ignore#P` on directory to mark local-only
  - `nosync` CLI tool (github.com/edtadros/icloud-nosync) can do this recursively
- Currently low risk: code repos appear to live outside Desktop/Documents
