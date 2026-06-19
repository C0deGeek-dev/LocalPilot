//! Compute a path relative to a base prefix. Original fixture for this repository.

/// The portion of `path` that follows `base`, without a leading `/`. Returns the
/// whole `path` when it does not start with `base`.
pub fn relative_to(base: &str, path: &str) -> String {
    match path.strip_prefix(base) {
        // BUG: the remainder still carries the leading separator.
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_sep() {
        assert_eq!(relative_to("a/b", "a/b/c.rs"), "c.rs");
    }

    #[test]
    fn returns_path_when_not_under_base() {
        assert_eq!(relative_to("x", "a/b/c.rs"), "a/b/c.rs");
    }

    #[test]
    fn handles_exact_match() {
        assert_eq!(relative_to("a/b", "a/b"), "");
    }
}
