use crate::{ItemKind, SemanticRole, StyledRange, TextStyle};

/// Builds presentation-only semantic ranges over the existing transcript text.
/// Markdown source remains the stable content-coordinate space.
pub(crate) fn semantic_ranges(kind: ItemKind, text: &str) -> Vec<StyledRange> {
    if text.is_empty() {
        return Vec::new();
    }
    if matches!(kind, ItemKind::Tool | ItemKind::Question | ItemKind::Shell) {
        return vec![range(0, text.len(), TextStyle::new(SemanticRole::Code))];
    }

    let mut output = Vec::new();
    let mut fenced = false;
    let mut line_start = 0usize;
    for line_with_newline in text.split_inclusive('\n') {
        let line_end = line_start.saturating_add(line_with_newline.trim_end_matches('\n').len());
        let line = &text[line_start..line_end];
        let trimmed = line.trim_start();
        let indent = line.len().saturating_sub(trimmed.len());
        if trimmed.starts_with("```") {
            output.push(range(
                line_start + indent,
                line_end,
                TextStyle::new(SemanticRole::Code),
            ));
            fenced = !fenced;
        } else if fenced {
            output.push(range(
                line_start,
                line_end,
                TextStyle::new(SemanticRole::Code),
            ));
        } else if heading_prefix_len(trimmed).is_some() {
            output.push(range(
                line_start + indent,
                line_end,
                TextStyle::new(SemanticRole::Heading).bold(),
            ));
        } else {
            if let Some(prefix_len) = list_prefix_len(trimmed) {
                output.push(range(
                    line_start + indent,
                    line_start + indent + prefix_len,
                    TextStyle::new(SemanticRole::Accent).bold(),
                ));
            }
            output.extend(inline_ranges(line, line_start));
            if line.matches('|').count() >= 2 {
                output.extend(line.match_indices('|').map(|(offset, value)| {
                    range(
                        line_start + offset,
                        line_start + offset + value.len(),
                        TextStyle::new(SemanticRole::Muted),
                    )
                }));
            }
        }
        line_start = line_start.saturating_add(line_with_newline.len());
    }
    if line_start < text.len() {
        output.extend(inline_ranges(&text[line_start..], line_start));
    }
    output.sort_by_key(|span| (span.start_byte, span.end_byte));
    output
}

fn inline_ranges(line: &str, base: usize) -> Vec<StyledRange> {
    let mut output = Vec::new();
    let mut cursor = 0usize;
    while cursor < line.len() {
        let rest = &line[cursor..];
        if let Some((start, end, style)) = next_inline(rest) {
            let absolute_start = base + cursor + start;
            let absolute_end = base + cursor + end;
            output.push(range(absolute_start, absolute_end, style));
            cursor = cursor.saturating_add(end.max(1));
        } else {
            break;
        }
    }
    output
}

fn next_inline(text: &str) -> Option<(usize, usize, TextStyle)> {
    let candidates = [
        inline_delimited(text, "**", TextStyle::new(SemanticRole::Assistant).bold()),
        inline_delimited(text, "`", TextStyle::new(SemanticRole::Code)),
        markdown_link(text),
        plain_link(text),
        inline_delimited(text, "*", TextStyle::new(SemanticRole::Assistant).italic()),
    ];
    candidates
        .into_iter()
        .enumerate()
        .filter_map(|(priority, candidate)| candidate.map(|value| (priority, value)))
        .min_by_key(|(priority, (start, _, _))| (*start, *priority))
        .map(|(_, value)| value)
}

fn inline_delimited(
    text: &str,
    delimiter: &str,
    style: TextStyle,
) -> Option<(usize, usize, TextStyle)> {
    let start = text.find(delimiter)?;
    let content_start = start + delimiter.len();
    let closing = text[content_start..].find(delimiter)?;
    let end = content_start + closing + delimiter.len();
    (end > content_start).then_some((start, end, style))
}

fn markdown_link(text: &str) -> Option<(usize, usize, TextStyle)> {
    let start = text.find('[')?;
    let label_end = text[start + 1..].find("](")? + start + 1;
    let end = text[label_end + 2..].find(')')? + label_end + 3;
    Some((start, end, TextStyle::new(SemanticRole::Link).underlined()))
}

fn plain_link(text: &str) -> Option<(usize, usize, TextStyle)> {
    let start = text.find("https://").or_else(|| text.find("http://"))?;
    let end = text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset);
    Some((start, end, TextStyle::new(SemanticRole::Link).underlined()))
}

fn heading_prefix_len(text: &str) -> Option<usize> {
    let hashes = text.bytes().take_while(|byte| *byte == b'#').count();
    (hashes > 0 && hashes <= 6 && text.as_bytes().get(hashes) == Some(&b' ')).then_some(hashes + 1)
}

fn list_prefix_len(text: &str) -> Option<usize> {
    if text.starts_with("- ") || text.starts_with("* ") || text.starts_with("> ") {
        return Some(2);
    }
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0 && text.get(digits..digits + 2) == Some(". ")).then_some(digits + 2)
}

fn range(start_byte: usize, end_byte: usize, style: TextStyle) -> StyledRange {
    StyledRange {
        start_byte,
        end_byte,
        style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_roles_are_original_semantic_ranges_over_stable_source() {
        let source = "# Heading\n- **bold** and `code` and [docs](https://example.test)\n";
        let styles = semantic_ranges(ItemKind::Assistant, source);
        assert!(styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Heading));
        assert!(styles.iter().any(|span| span.style.bold));
        assert!(styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Code));
        assert!(styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Link));
        assert!(styles.iter().all(|span| {
            span.start_byte <= span.end_byte
                && span.end_byte <= source.len()
                && source.is_char_boundary(span.start_byte)
                && source.is_char_boundary(span.end_byte)
        }));
    }

    #[test]
    fn fenced_code_and_table_separators_receive_semantic_roles() {
        let source = "```rust\nfn main() {}\n```\n| a | b |\n";
        let styles = semantic_ranges(ItemKind::Assistant, source);
        assert!(styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Code && span.end_byte > 10));
        assert!(styles
            .iter()
            .any(|span| span.style.role == SemanticRole::Muted));
    }
}
