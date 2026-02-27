/// Returns true if a tmutil exit code is a "safe" non-fatal error.
///
/// Known safe codes:
/// - 213: path not found (item was already removed / never existed)
pub fn is_tmutil_safe_error(exit_code: i32) -> bool {
    matches!(exit_code, 213)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exit_code_213_is_safe() {
        assert!(is_tmutil_safe_error(213));
    }

    #[test]
    fn test_other_exit_codes_are_not_safe() {
        assert!(!is_tmutil_safe_error(0));
        assert!(!is_tmutil_safe_error(1));
        assert!(!is_tmutil_safe_error(-1));
        assert!(!is_tmutil_safe_error(127));
        assert!(!is_tmutil_safe_error(212));
        assert!(!is_tmutil_safe_error(214));
    }
}
