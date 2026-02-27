# Agent Guidelines

## Pre-commit checks

Before creating any commit, always run:

```bash
cargo fmt --all
cargo clippy --all-features
```

Fix any issues before committing. Do not skip these checks.

## Testing policy

### No trivial tests

Do not write tests that fall into any of these categories:

- **Testing third-party crate behavior** — e.g., verifying that `toml::from_str` parses valid TOML, that `path_clean::clean` resolves `..` components, or that `dirs::home_dir` returns a home directory. These test the library, not our code.
- **Restating the implementation** — e.g., testing a one-liner `matches!(exit_code, 22 | 213)` with `assert!(is_safe(22)); assert!(is_safe(213))` just duplicates the match arms. If the test reads like a copy of the function body, it is trivial.
- **Verifying struct constructors or Default impls** — e.g., asserting that `Cache::empty()` returns `version: 1` and empty `paths`. The constructor is a struct literal; the test adds no value.
- **Asserting mathematical tautologies** — e.g., `diff_sets(A, A)` returns empty. Set A minus set A is always empty by definition.
- **Checking obvious error/fallback branches** — e.g., verifying that loading a missing file returns a default. If the fallback is a one-liner, testing it just restates the code.

Every test should exercise meaningful application logic — multi-step workflows, non-trivial state transitions, edge cases in business logic, or integration between components.
