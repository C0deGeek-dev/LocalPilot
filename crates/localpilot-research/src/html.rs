//! A small, dependency-free HTML-to-text reducer.
//!
//! Web evidence arrives as a raw HTML document. Pasting that into a finding (or
//! its evidence block) surfaces script/style bodies, markup, and entities as
//! "junk" and blows the length budget on chrome rather than content. This turns
//! a document into readable prose: it drops whole non-content elements
//! (`<script>`, `<style>`, `<head>`, …) *including their bodies* — which a naive
//! `<…>` tag strip leaves behind — inserts line breaks at block boundaries,
//! removes the remaining tags, decodes common entities, and collapses
//! whitespace.
//!
//! It is intentionally not a spec-conformant HTML parser (that would be a heavy
//! dependency for a best-effort display reduction); it is a robust,
//! host-neutral, pure heuristic that never panics on malformed input.

use crate::flatten_whitespace;

/// Elements whose entire body is non-prose and must be removed wholesale, not
/// merely have their tags stripped (stripping `<script>`/`</script>` alone
/// would leave the JavaScript between them as text — the exact "html junk"
/// this reducer exists to prevent). `head`/`nav`/`footer` are page chrome,
/// dropped to keep the extracted text focused on content.
const DROPPED_ELEMENTS: [&str; 11] = [
    "script", "style", "head", "noscript", "svg", "template", "iframe", "canvas", "nav", "footer",
    "form",
];

/// Reduce an HTML document to readable plain text.
///
/// Deterministic and total: any input yields a string, never a panic. On text
/// that isn't actually HTML it degrades to a whitespace-collapsed copy (there
/// are no elements to drop and no tags to strip), so a caller that mistakes a
/// plain-text body for HTML loses nothing but redundant whitespace.
#[must_use]
pub fn html_to_text(html: &str) -> String {
    let without_comments = strip_comments(html);
    let mut body = without_comments;
    for element in DROPPED_ELEMENTS {
        body = strip_element(&body, element);
    }
    let stripped = strip_tags_with_breaks(&body);
    let decoded = decode_entities(&stripped);
    collapse_lines(&decoded)
}

/// Remove `<!-- … -->` comment spans. A comment with no terminator drops the
/// remainder of the document (it can never be content).
fn strip_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Remove every `<tag …>…</tag>` element — body included — for one tag name,
/// case-insensitively. An unterminated element drops the rest of the input (it
/// cannot be closed, so nothing after its open tag is trustworthy content).
fn strip_element(input: &str, tag: &str) -> String {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let Some(rel) = lower[i..].find(&open) else {
            out.push_str(&input[i..]);
            break;
        };
        let start = i + rel;
        // Only a real tag boundary counts: the char after `<tag` must end the
        // name (`>`, `/`, whitespace) so `<script>` matches but `<scripted>`
        // does not. On a miss, advance past the `<` and re-scan.
        let after = input[start + open.len()..].chars().next();
        let is_tag = matches!(after, Some(c) if c == '>' || c == '/' || c.is_whitespace())
            || after.is_none();
        if !is_tag {
            out.push_str(&input[i..start + 1]);
            i = start + 1;
            continue;
        }
        out.push_str(&input[i..start]);
        match lower[start..].find(&close) {
            Some(crel) => i = start + crel + close.len(),
            None => break, // unterminated: drop the remainder
        }
    }
    out
}

/// Block-level tags whose boundary should become a line break, so text from
/// adjacent blocks does not run together into one word-salad line.
const BLOCK_TAGS: [&str; 24] = [
    "p",
    "div",
    "br",
    "hr",
    "li",
    "ul",
    "ol",
    "tr",
    "td",
    "table",
    "section",
    "article",
    "header",
    "aside",
    "blockquote",
    "pre",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "figure",
    "dl",
];

/// Strip all remaining tags, emitting a newline where a block-level tag sat so
/// block structure survives as line breaks. A stray `<` with no closing `>` is
/// treated as literal text rather than swallowing the rest of the document.
fn strip_tags_with_breaks(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let tail = &rest[lt..];
        let Some(gt) = tail.find('>') else {
            // No terminator: the '<' is literal text, keep it and stop scanning.
            out.push_str(tail);
            return out;
        };
        if is_block_tag(&tail[..=gt]) {
            out.push('\n');
        }
        rest = &tail[gt + 1..];
    }
    out.push_str(rest);
    out
}

/// Whether a full tag token (`<…>`) names a block-level element, ignoring a
/// leading `/` (closing tag) and reading the alphabetic name only.
fn is_block_tag(tag: &str) -> bool {
    let inner = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start_matches('/')
        .trim();
    let name: String = inner
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    BLOCK_TAGS.contains(&name.as_str())
}

/// Decode the handful of HTML entities common in prose, plus numeric
/// (`&#123;`) and hex (`&#x1f;`) character references. An unrecognised or
/// malformed entity is left verbatim rather than guessed at.
fn decode_entities(input: &str) -> String {
    if !input.contains('&') {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match tail.find(';').filter(|&end| end <= MAX_ENTITY_LEN) {
            Some(end) => {
                let entity = &tail[1..end];
                match decode_entity(entity) {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&tail[..=end]),
                }
                rest = &tail[end + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Upper bound on the length of an entity body (between `&` and `;`) we will
/// try to decode, so a stray `&` in prose does not scan arbitrarily far.
const MAX_ENTITY_LEN: usize = 10;

/// Decode one entity body (the text between `&` and `;`). `None` if unknown.
fn decode_entity(body: &str) -> Option<char> {
    match body {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" | "#39" => Some('\''),
        "nbsp" => Some(' '),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "hellip" => Some('…'),
        "copy" => Some('©'),
        "reg" => Some('®'),
        "trade" => Some('™'),
        _ => decode_numeric_entity(body),
    }
}

/// Decode a numeric character reference body: `#123` (decimal) or `#x1f`/`#X1F`
/// (hex). `None` on a non-numeric body or an out-of-range code point.
fn decode_numeric_entity(body: &str) -> Option<char> {
    let digits = body.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

/// Collapse whitespace: flatten each line's internal runs to single spaces and
/// drop lines left empty, so block breaks survive as single newlines and
/// nothing renders as a wall of blank space.
fn collapse_lines(input: &str) -> String {
    input
        .lines()
        .map(flatten_whitespace)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_script_and_style_bodies_not_just_their_tags() {
        // The bug this prevents: a naive `<…>` strip leaves the JavaScript
        // between `<script>` and `</script>` as text — the "html junk" a
        // reviewer saw pasted into a research summary.
        let html = "<html><head><title>T</title><style>.a{color:red}</style></head>\
             <body><script>var x = validRedirects.get(file); viewer.srcdoc = y;</script>\
             <p>Caches speed up repeated reads.</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Caches speed up repeated reads."));
        assert!(
            !text.contains("validRedirects"),
            "script body removed: {text}"
        );
        assert!(!text.contains("color:red"), "style body removed: {text}");
        assert!(!text.contains('<'), "no markup remains: {text}");
        assert!(!text.contains("srcdoc"));
    }

    #[test]
    fn block_tags_become_line_breaks() {
        let html = "<p>one</p><p>two</p><div>three</div>";
        let text = html_to_text(html);
        assert_eq!(text, "one\ntwo\nthree");
    }

    #[test]
    fn decodes_named_numeric_and_hex_entities() {
        let html = "<p>Tom &amp; Jerry &lt;3 &#39;hi&#39; &#x2764; caf&#233;</p>";
        let text = html_to_text(html);
        assert_eq!(text, "Tom & Jerry <3 'hi' ❤ café");
    }

    #[test]
    fn unknown_entity_is_left_verbatim() {
        assert_eq!(html_to_text("<p>a &bogus; b</p>"), "a &bogus; b");
        // A bare ampersand in prose is not an entity and stays put.
        assert_eq!(html_to_text("<p>Q&A session</p>"), "Q&A session");
    }

    #[test]
    fn unterminated_script_drops_the_remainder() {
        let html = "<p>keep</p><script>secret() never closed";
        assert_eq!(html_to_text(html), "keep");
    }

    #[test]
    fn comments_are_removed() {
        assert_eq!(html_to_text("<p>a<!-- hidden -->b</p>"), "ab");
    }

    #[test]
    fn non_html_text_survives_as_plain_text() {
        // A plain-text body run through the reducer only has its whitespace
        // collapsed — nothing is lost.
        let text = html_to_text("just plain text,\n  no tags at all");
        assert_eq!(text, "just plain text,\nno tags at all");
    }

    #[test]
    fn does_not_confuse_a_non_dropped_tag_prefix() {
        // `<sectioned>` is not `<section>`; the boundary check must not treat
        // `<script...>`-style prefixes loosely. Here `<span>` shares no prefix,
        // but assert a near-miss on a dropped name is handled: `<scripted>` is
        // not `<script>` and must not trigger whole-element removal.
        let html = "<p>before</p><scripted>middle</scripted><p>after</p>";
        let text = html_to_text(html);
        assert!(text.contains("before"));
        assert!(text.contains("middle"), "non-script element kept: {text}");
        assert!(text.contains("after"));
    }

    #[test]
    fn empty_input_is_empty() {
        assert_eq!(html_to_text(""), "");
    }
}
