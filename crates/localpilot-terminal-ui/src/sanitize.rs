/// Normalize line endings and remove terminal control sequences from text
/// before it enters application state.
///
/// Ratatui cells are not an escaping boundary: adjacent cell symbols are
/// written contiguously by a terminal backend, so an ESC/CSI payload stored as
/// ordinary content could become a live terminal command. Incomplete trailing
/// escape sequences are dropped rather than carried into a later payload.
#[must_use]
pub fn sanitize_text(text: &str) -> String {
    let dirty = |character: char| {
        (character.is_ascii_control() && character != '\n' && character != '\t')
            || ('\u{80}'..='\u{9f}').contains(&character)
    };
    if !text.chars().any(dirty) {
        return text.to_string();
    }

    let mut output = String::with_capacity(text.len());
    let mut characters = text.char_indices().peekable();
    while let Some((_, character)) = characters.next() {
        match character {
            '\r' => {
                if matches!(characters.peek(), Some((_, '\n'))) {
                    characters.next();
                }
                output.push('\n');
            }
            '\n' | '\t' => output.push(character),
            '\u{1b}' => consume_escape_sequence(&mut characters),
            character
                if character.is_ascii_control() || ('\u{80}'..='\u{9f}').contains(&character) => {}
            character => output.push(character),
        }
    }
    output
}

fn consume_escape_sequence(characters: &mut std::iter::Peekable<std::str::CharIndices<'_>>) {
    match characters.peek().map(|&(_, character)| character) {
        Some('[') => {
            characters.next();
            for (_, character) in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    break;
                }
            }
        }
        Some(']') => {
            characters.next();
            while let Some((_, character)) = characters.next() {
                if character == '\u{7}' {
                    break;
                }
                if character == '\u{1b}' {
                    if matches!(characters.peek(), Some((_, '\\'))) {
                        characters.next();
                    }
                    break;
                }
            }
        }
        Some(_) => {
            characters.next();
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_terminal_commands_and_control_bytes() {
        let input = "before\x1b[2Jafter\x1b]0;owned\x07!\0\u{009b}31m";
        let clean = sanitize_text(input);
        assert_eq!(clean, "beforeafter!31m");
        assert!(!clean.chars().any(char::is_control));
    }

    #[test]
    fn preserves_newlines_and_tabs_and_normalizes_carriage_returns() {
        assert_eq!(
            sanitize_text("one\r\ntwo\rthree\tfour"),
            "one\ntwo\nthree\tfour"
        );
    }

    #[test]
    fn drops_an_incomplete_trailing_escape() {
        assert_eq!(sanitize_text("safe\x1b[31"), "safe");
    }
}
