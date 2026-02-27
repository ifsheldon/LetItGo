/// Returns true if a tmutil exit code is a "safe" non-fatal error.
///
/// Known safe codes:
/// - 22: cannot change exclusion setting (path no longer exists on disk)
/// - 213: path not found (item was already removed / never existed)
pub fn is_tmutil_safe_error(exit_code: i32) -> bool {
    matches!(exit_code, 22 | 213)
}
