//! Classify a text token. Original fixture for this repository.

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Empty,
    Number,
    Word,
}

/// Sort `token` into its [`Kind`].
pub fn classify(token: &str) -> Kind {
    // BUG: the empty case must come first; an empty string has no non-digit
    // characters, so it falls through to `Word`.
    if token.chars().all(|c| c.is_ascii_digit()) && !token.is_empty() {
        Kind::Number
    } else {
        Kind::Word
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_empty() {
        assert_eq!(classify(""), Kind::Empty);
    }

    #[test]
    fn digits_are_a_number() {
        assert_eq!(classify("42"), Kind::Number);
    }

    #[test]
    fn letters_are_a_word() {
        assert_eq!(classify("hi"), Kind::Word);
    }
}
