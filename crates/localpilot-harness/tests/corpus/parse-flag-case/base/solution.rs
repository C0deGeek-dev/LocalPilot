//! Parse a boolean flag from text. Original fixture for this repository.

/// Parse a human-written boolean flag. Accepts `true`/`false`/`yes`/`no`
/// case-insensitively, ignoring surrounding whitespace.
pub fn parse_flag(s: &str) -> Option<bool> {
    // BUG: no trimming, no case folding — only exact lowercase spellings match.
    match s {
        "true" | "yes" => Some(true),
        "false" | "no" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_lowercase() {
        assert_eq!(parse_flag("true"), Some(true));
        assert_eq!(parse_flag("no"), Some(false));
    }

    #[test]
    fn parses_uppercase_and_trims() {
        assert_eq!(parse_flag("TRUE"), Some(true));
        assert_eq!(parse_flag("  Yes "), Some(true));
        assert_eq!(parse_flag("False"), Some(false));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_flag("maybe"), None);
    }
}
