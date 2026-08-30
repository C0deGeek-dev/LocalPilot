//! A small, dependency-free HTML reducer: to Markdown for evidence, to plain
//! text for one-line excerpts.
//!
//! Web evidence arrives as a raw HTML document. Pasting that into a finding (or
//! its evidence block) surfaces script/style bodies, markup, and entities as
//! "junk" and blows the length budget on chrome rather than content.
//! [`html_to_markdown`] turns a document into readable Markdown: it drops whole
//! non-content elements (`<script>`, `<style>`, `<head>`, …) *including their
//! bodies* — which a naive `<…>` tag strip leaves behind — and maps the
//! structural tags a reader (and a model) actually uses onto Markdown: headings,
//! lists, links, inline and fenced code, emphasis, and block breaks.
//! [`html_to_text`] is the flat variant for contexts where structure has no
//! room (a single-line excerpt), and [`markdown_to_text`] flattens Markdown
//! syntax back out of text destined for those same one-line homes.
//!
//! It is intentionally not a spec-conformant HTML parser (that would be a heavy
//! dependency for a best-effort display reduction); it is a robust,
//! host-neutral, pure heuristic that never panics on malformed input.

use crate::flatten_whitespace;
use crate::output::backtick_fence;

/// Elements whose entire body is non-prose and must be removed wholesale, not
/// merely have their tags stripped (stripping `<script>`/`</script>` alone
/// would leave the JavaScript between them as text — the exact "html junk"
/// this reducer exists to prevent). `head`/`nav`/`footer` are page chrome,
/// dropped to keep the extracted text focused on content.
const DROPPED_ELEMENTS: [&str; 11] = [
    "script", "style", "head", "noscript", "svg", "template", "iframe", "canvas", "nav", "footer",
    "form",
];

/// Reduce an HTML document to readable Markdown.
///
/// Headings become `#` headings, `<li>` becomes a `- ` item, `<a href>` becomes
/// a `[text](url)` link, `<pre>` becomes a fenced code block (its whitespace
/// preserved, the fence sized past any backtick run inside), `<code>` becomes
/// inline code, and `<strong>`/`<em>` become `**`/`*`. Everything the
/// plain-text reducer drops (script/style bodies, chrome elements, comments,
/// entities) is dropped or decoded identically.
///
/// Deterministic and total: any input yields a string, never a panic. On text
/// that isn't actually HTML it degrades to a whitespace-collapsed copy (in
/// HTML a source newline is formatting, not a line break), so a caller that
/// mistakes a plain-text body for HTML loses line positions but never a word.
#[must_use]
pub fn html_to_markdown(html: &str) -> String {
    let without_comments = strip_comments(html);
    let mut body = without_comments;
    for element in DROPPED_ELEMENTS {
        body = strip_element(&body, element);
    }
    render_segments(convert_to_segments(&body))
}

/// Reduce an HTML document to readable plain text.
///
/// The flat sibling of [`html_to_markdown`], for contexts where structure has
/// no room (one-line excerpts). Deterministic and total: any input yields a
/// string, never a panic. On text that isn't actually HTML it degrades to a
/// whitespace-collapsed copy.
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

/// Flatten Markdown syntax out of text destined for a one-line home: fence
/// marker lines are dropped (their code content is kept — it may be the
/// substance), leading heading/list/quote markers are stripped, inline
/// backticks are removed, and `[text](url)`/`![alt](url)` collapse to their
/// text. Total and panic-free; non-Markdown text passes through unchanged.
#[must_use]
pub fn markdown_to_text(markdown: &str) -> String {
    markdown
        .lines()
        .filter(|line| !is_fence_line(line))
        .map(|line| strip_link_syntax(strip_line_markers(line)).replace('`', ""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether a line is a code-fence marker (` ``` `-prefixed, any fence length,
/// optionally with an info string).
fn is_fence_line(line: &str) -> bool {
    line.trim_start().starts_with("```")
}

/// Strip a leading heading (`# `), blockquote (`> `), or list (`- `, `* `,
/// `+ `, `1. `) marker from one line, returning the readable remainder.
fn strip_line_markers(line: &str) -> &str {
    let trimmed = line.trim_start();
    let after_hashes = trimmed.trim_start_matches('#');
    if after_hashes.len() < trimmed.len() {
        if let Some(text) = after_hashes.strip_prefix(' ') {
            return text;
        }
    }
    if let Some(text) = trimmed.strip_prefix("> ") {
        return text;
    }
    for marker in ["- ", "* ", "+ "] {
        if let Some(text) = trimmed.strip_prefix(marker) {
            return text;
        }
    }
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(text) = trimmed[digits..].strip_prefix(". ") {
            return text;
        }
    }
    trimmed
}

/// Collapse `[text](url)` and `![alt](url)` spans to their text. A `[` that is
/// not part of link syntax is kept literally.
fn strip_link_syntax(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let head = &rest[..open];
        // An image marker (`![`) drops its `!` along with the syntax.
        let head = head.strip_suffix('!').unwrap_or(head);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push_str(&rest[..open]);
            out.push('[');
            rest = after;
            continue;
        };
        let after_close = &after[close + 1..];
        if let Some(target) = after_close.strip_prefix('(') {
            if let Some(paren) = target.find(')') {
                out.push_str(head);
                out.push_str(&after[..close]);
                rest = &target[paren + 1..];
                continue;
            }
        }
        out.push_str(&rest[..open]);
        out.push('[');
        rest = after;
    }
    out.push_str(rest);
    out
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

/// Extract the `src` URLs of `<iframe>` elements, in document order, deduped.
///
/// The reducer drops the iframe element wholesale (its body is not the parent's
/// prose — see [`DROPPED_ELEMENTS`]), but a documentation iframe's *source* is a
/// real lead the research path can gate through the `[research.web]` allowlist
/// and fetch through the ordinary path. Only navigable srcs are returned
/// (`http(s)`, protocol-relative, or relative — resolved against the parent by
/// the caller); `srcdoc`, `about:blank`, `javascript:`, and `data:` are skipped
/// (inline `srcdoc` content is the renderer's job, not a fetchable lead).
/// Entities in the URL are decoded. Total and panic-free.
#[must_use]
pub fn iframe_sources(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while let Some(rel) = lower[i..].find("<iframe") {
        let start = i + rel;
        // Confirm a real tag boundary after `<iframe` so `<iframexyz>` is not a
        // match; on a miss advance past the `<` and re-scan.
        let after = html[start + "<iframe".len()..].chars().next();
        let is_tag = matches!(after, Some(c) if c == '>' || c == '/' || c.is_whitespace())
            || after.is_none();
        if !is_tag {
            i = start + "<iframe".len();
            continue;
        }
        let tag_end = lower[start..].find('>').map_or(html.len(), |e| start + e);
        let tag = &html[start..tag_end];
        if let Some(src) = extract_attr(tag, "src") {
            let decoded = decode_entities(src.trim());
            let src = decoded.trim();
            if is_navigable_src(src) && seen.insert(src.to_string()) {
                out.push(src.to_string());
            }
        }
        i = tag_end.max(start + "<iframe".len());
    }
    out
}

/// Read one attribute's value out of a single start-tag string
/// (e.g. `<iframe src="x">`). Case-insensitive name; the match must sit on an
/// attribute boundary (so `srcdoc`/`data-src` do not match `src`) and be
/// followed by `=`. Tolerates double-, single-, and unquoted values. `None`
/// when the attribute is absent or valueless.
fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(name) {
        let at = search + rel;
        let boundary_before = at == 0
            || matches!(
                bytes[at - 1],
                b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'<' | b'/'
            );
        let after_name = &tag[at + name.len()..];
        if boundary_before {
            let eq = after_name.trim_start();
            if let Some(value) = eq.strip_prefix('=') {
                return Some(read_attr_value(value.trim_start()));
            }
        }
        search = at + name.len();
    }
    None
}

/// Read an attribute value beginning at `s` (leading whitespace already
/// trimmed): a `"…"`/`'…'` quoted run, or an unquoted token up to the next
/// whitespace or `>`.
fn read_attr_value(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(quote @ ('"' | '\'')) => {
            let inner = &s[quote.len_utf8()..];
            match inner.find(quote) {
                Some(end) => inner[..end].to_string(),
                None => inner.to_string(),
            }
        }
        _ => s
            .split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
            .to_string(),
    }
}

/// Whether an iframe `src` is a navigable lead worth gating and fetching, as
/// opposed to an inline/non-navigable scheme the fetch path cannot use.
fn is_navigable_src(src: &str) -> bool {
    if src.is_empty() {
        return false;
    }
    let lower = src.to_ascii_lowercase();
    !(lower.starts_with("javascript:") || lower.starts_with("data:") || lower == "about:blank")
}

// --- Markdown conversion ------------------------------------------------------

/// A converted span of the document: prose (whitespace-tidied at render) or a
/// code block from `<pre>` (whitespace preserved verbatim, fenced at render).
enum Segment {
    Prose(String),
    Code(String),
}

/// Accumulates converted output. Source text is whitespace-normalized as it is
/// emitted; structural Markdown markers (`\n\n`, `# `, `- `, `[`, …) are
/// emitted raw so the final tidy pass keeps them intact.
struct MarkdownWriter {
    segments: Vec<Segment>,
    prose: String,
    /// Open `<a>` elements: `Some(href)` when the anchor became a link (its
    /// close emits `](href)`), `None` when it did not (its close emits nothing).
    links: Vec<Option<String>>,
}

impl MarkdownWriter {
    fn new() -> Self {
        Self {
            segments: Vec::new(),
            prose: String::new(),
            links: Vec::new(),
        }
    }

    /// Emit source text: entities decoded, each whitespace run (newlines
    /// included — HTML source indentation is not content) collapsed to one space.
    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let decoded = decode_entities(raw);
        let mut last_was_space = false;
        for ch in decoded.chars() {
            if ch.is_whitespace() {
                if !last_was_space {
                    self.prose.push(' ');
                    last_was_space = true;
                }
            } else {
                self.prose.push(ch);
                last_was_space = false;
            }
        }
    }

    /// Emit a structural Markdown marker verbatim.
    fn marker(&mut self, marker: &str) {
        self.prose.push_str(marker);
    }

    /// Emit a `<pre>` body as its own code segment, flushing pending prose.
    fn code_block(&mut self, code: String) {
        self.flush_prose();
        self.segments.push(Segment::Code(code));
    }

    fn flush_prose(&mut self) {
        if !self.prose.is_empty() {
            self.segments
                .push(Segment::Prose(std::mem::take(&mut self.prose)));
        }
    }

    fn finish(mut self) -> Vec<Segment> {
        self.flush_prose();
        self.segments
    }
}

/// Walk the (comment- and chrome-free) document, mapping tags to Markdown.
fn convert_to_segments(body: &str) -> Vec<Segment> {
    let mut writer = MarkdownWriter::new();
    let mut rest = body;
    while let Some(lt) = rest.find('<') {
        writer.text(&rest[..lt]);
        let tail = &rest[lt..];
        let Some(gt) = tail.find('>') else {
            // No terminator: the '<' is literal text, keep it and stop scanning.
            writer.text(tail);
            return writer.finish();
        };
        let token = &tail[..=gt];
        // `<pre>` is special: its body keeps whitespace verbatim and becomes a
        // fenced code block. An unterminated `<pre>` takes the remainder — the
        // content is real, unlike an unterminated script.
        if tag_name(token) == "pre" && !is_closing_tag(token) {
            let after = &tail[gt + 1..];
            let lower = after.to_ascii_lowercase();
            let (inner, next) = match lower.find("</pre>") {
                Some(end) => (&after[..end], &after[end + "</pre>".len()..]),
                None => (after, ""),
            };
            writer.code_block(code_text(inner));
            rest = next;
            continue;
        }
        emit_tag(&mut writer, token);
        rest = &tail[gt + 1..];
    }
    writer.text(rest);
    writer.finish()
}

/// Map one full tag token (`<…>`) onto its Markdown marker(s).
fn emit_tag(writer: &mut MarkdownWriter, token: &str) {
    let closing = is_closing_tag(token);
    let name = tag_name(token);
    match name.as_str() {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            if closing {
                writer.marker("\n\n");
            } else {
                let level = name.trim_start_matches('h').parse::<usize>().unwrap_or(1);
                writer.marker("\n\n");
                writer.marker(&"#".repeat(level));
                writer.marker(" ");
            }
        }
        "li" => {
            if !closing {
                writer.marker("\n- ");
            }
        }
        "br" => writer.marker("\n"),
        "hr" => writer.marker("\n\n---\n\n"),
        "a" => {
            if closing {
                if let Some(Some(href)) = writer.links.pop() {
                    writer.marker(&format!("]({href})"));
                }
            } else {
                match link_href(token) {
                    Some(href) => {
                        writer.links.push(Some(href));
                        writer.marker("[");
                    }
                    None => writer.links.push(None),
                }
            }
        }
        "code" => writer.marker("`"),
        "strong" | "b" => writer.marker("**"),
        "em" | "i" => writer.marker("*"),
        "blockquote" => {
            if closing {
                writer.marker("\n\n");
            } else {
                writer.marker("\n\n> ");
            }
        }
        "td" | "th" => {
            if closing {
                writer.marker(" | ");
            }
        }
        "tr" => writer.marker("\n"),
        _ if is_block_tag(token) => writer.marker("\n\n"),
        _ => {}
    }
}

/// Whether a full tag token is a closing tag (`</…>`).
fn is_closing_tag(token: &str) -> bool {
    token.trim_start_matches('<').trim_start().starts_with('/')
}

/// The lowercase alphanumeric name of a full tag token, ignoring a leading `/`.
fn tag_name(token: &str) -> String {
    token
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim_start_matches('/')
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Extract the `href` of an opening `<a …>` token when it is a followable
/// link. `None` for a missing/empty href, a fragment (`#…`), or a
/// `javascript:` pseudo-URL — those anchors contribute their text only.
fn link_href(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    let mut search_from = 0;
    let idx = loop {
        let rel = lower[search_from..].find("href")?;
        let at = search_from + rel;
        // A real attribute name boundary: `href` must not be the tail of a
        // longer name like `data-href`.
        let boundary = at == 0
            || !lower[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if boundary {
            break at;
        }
        search_from = at + 4;
    };
    let rest = token[idx + 4..].trim_start().strip_prefix('=')?;
    let rest = rest.trim_start();
    let href = if let Some(quoted) = rest.strip_prefix('"') {
        quoted.split('"').next().unwrap_or("")
    } else if let Some(quoted) = rest.strip_prefix('\'') {
        quoted.split('\'').next().unwrap_or("")
    } else {
        rest.split(|c: char| c.is_whitespace() || c == '>')
            .next()
            .unwrap_or("")
    };
    let href = decode_entities(href.trim());
    if href.is_empty()
        || href.starts_with('#')
        || href.to_ascii_lowercase().starts_with("javascript:")
    {
        return None;
    }
    Some(href)
}

/// The text of a `<pre>` body: inner tags stripped (no breaks inserted — the
/// body's own newlines are the structure), entities decoded, whitespace kept.
fn code_text(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let tail = &rest[lt..];
        let Some(gt) = tail.find('>') else {
            out.push_str(tail);
            return decode_entities(&out);
        };
        rest = &tail[gt + 1..];
    }
    out.push_str(rest);
    decode_entities(&out)
}

/// Render converted segments: prose is whitespace-tidied (line ends trimmed,
/// blank-line runs collapsed to one), code is fenced verbatim with a fence
/// longer than any backtick run inside. Segments join with one blank line.
fn render_segments(segments: Vec<Segment>) -> String {
    let mut out = String::new();
    for segment in segments {
        let rendered = match segment {
            Segment::Prose(prose) => tidy_prose(&prose),
            Segment::Code(code) => {
                let code = code.trim_matches('\n');
                if code.is_empty() {
                    String::new()
                } else {
                    let fence = backtick_fence(code);
                    format!("{fence}\n{code}\n{fence}")
                }
            }
        };
        if rendered.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&rendered);
    }
    out
}

/// Tidy a prose span: trim each line's edges, collapse internal space runs,
/// and collapse runs of blank lines to a single blank line (so block breaks
/// read as paragraphs, never as walls of empty space).
fn tidy_prose(prose: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut blank_pending = false;
    for line in prose.lines() {
        let flat = flatten_whitespace(line);
        if flat.is_empty() {
            blank_pending = !lines.is_empty();
            continue;
        }
        if blank_pending {
            lines.push(String::new());
            blank_pending = false;
        }
        lines.push(flat);
    }
    lines.join("\n")
}

// --- plain-text reduction -----------------------------------------------------

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
    BLOCK_TAGS.contains(&tag_name(tag).as_str())
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
    fn iframe_sources_extracts_navigable_srcs_in_order_deduped() {
        let html = "<div><iframe src=\"https://docs.example/ch1\"></iframe>\
                    <iframe src='https://docs.example/ch2'></iframe>\
                    <iframe src=\"https://docs.example/ch1\"></iframe>\
                    <iframe srcdoc=\"<p>inline</p>\"></iframe>\
                    <iframe src=about:blank></iframe>\
                    <iframe src=\"javascript:void(0)\"></iframe>\
                    <iframe></iframe></div>";
        let srcs = iframe_sources(html);
        assert_eq!(
            srcs,
            vec![
                "https://docs.example/ch1".to_string(),
                "https://docs.example/ch2".to_string(),
            ],
            "only navigable srcs, order-preserving, deduped; srcdoc/about:blank/js skipped"
        );
    }

    #[test]
    fn iframe_sources_decodes_entities_and_ignores_lookalikes() {
        // `data-src` must not match `src`; `&amp;` decodes.
        let html = "<iframe data-src=\"https://x.example/nope\" \
                    src=\"https://x.example/a?p=1&amp;q=2\"></iframe>";
        assert_eq!(
            iframe_sources(html),
            vec!["https://x.example/a?p=1&q=2".to_string()]
        );
    }

    #[test]
    fn iframe_sources_survives_reduction_that_drops_the_element_body() {
        // The reducer still removes the iframe body as junk, but the src is
        // recoverable as a lead (LocalHub#37).
        let html = "<body><nav>menu</nav>\
                    <iframe src=\"https://frame.example/doc\">fallback text</iframe></body>";
        assert_eq!(
            iframe_sources(html),
            vec!["https://frame.example/doc".to_string()]
        );
        let reduced = html_to_text(html);
        assert!(
            !reduced.contains("fallback text"),
            "body still dropped: {reduced}"
        );
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
        assert_eq!(html_to_markdown(""), "");
    }

    // --- Markdown conversion ---------------------------------------------------

    #[test]
    fn headings_lists_and_paragraphs_become_markdown_structure() {
        let html = "<h1>Guide</h1><p>Intro text.</p><h2>Steps</h2>\
             <ul><li>first</li><li>second</li></ul>";
        let md = html_to_markdown(html);
        assert_eq!(
            md,
            "# Guide\n\nIntro text.\n\n## Steps\n\n- first\n- second"
        );
    }

    #[test]
    fn anchors_become_links_and_useless_anchors_keep_their_text() {
        let html = "<p>See <a href=\"https://docs.rs/tokio\">the tokio docs</a> \
             and <a href=\"#top\">back to top</a> or <a>no href</a>.</p>";
        let md = html_to_markdown(html);
        assert!(
            md.contains("[the tokio docs](https://docs.rs/tokio)"),
            "href becomes a Markdown link: {md}"
        );
        assert!(
            md.contains("back to top") && !md.contains("(#top)"),
            "a fragment anchor keeps only its text: {md}"
        );
        assert!(md.contains("no href"));
        assert!(!md.contains('<'), "no markup remains: {md}");
    }

    #[test]
    fn pre_becomes_a_fenced_block_with_whitespace_preserved() {
        let html = "<p>Example:</p><pre><code>fn main() {\n    println!(\"hi\");\n}</code></pre>";
        let md = html_to_markdown(html);
        assert!(
            md.contains("```\nfn main() {\n    println!(\"hi\");\n}\n```"),
            "pre body is fenced verbatim, indentation kept: {md}"
        );
    }

    #[test]
    fn pre_containing_backticks_gets_a_longer_fence() {
        let html = "<pre>a ``` fence inside</pre>";
        let md = html_to_markdown(html);
        assert!(
            md.starts_with("````"),
            "fence is sized past inner backtick runs: {md}"
        );
    }

    #[test]
    fn inline_code_and_emphasis_map_to_markdown() {
        let html =
            "<p>Use <code>cargo test</code> — it is <strong>fast</strong> and <em>simple</em>.</p>";
        let md = html_to_markdown(html);
        assert!(md.contains("`cargo test`"), "inline code: {md}");
        assert!(md.contains("**fast**"), "bold: {md}");
        assert!(md.contains("*simple*"), "italic: {md}");
    }

    #[test]
    fn markdown_conversion_drops_chrome_like_the_text_reducer() {
        let html = "<head><title>T</title></head><nav><a href=\"/home\">Home</a></nav>\
             <script>track();</script><p>Real content.</p><footer>© corp</footer>";
        let md = html_to_markdown(html);
        assert_eq!(md, "Real content.");
    }

    #[test]
    fn blank_line_runs_collapse_and_entities_decode_in_markdown() {
        let html = "<div><div><p>Tom &amp; Jerry</p></div></div><p>End.</p>";
        let md = html_to_markdown(html);
        assert_eq!(md, "Tom & Jerry\n\nEnd.");
    }

    #[test]
    fn unterminated_pre_keeps_the_remainder_as_code() {
        // Unlike an unterminated script (junk), an unterminated pre holds real
        // content — keep it rather than dropping the tail of the page.
        let html = "<p>before</p><pre>let x = 1;";
        let md = html_to_markdown(html);
        assert!(md.contains("before"));
        assert!(md.contains("let x = 1;"), "pre tail kept: {md}");
    }

    #[test]
    fn table_cells_stay_separated() {
        let html =
            "<table><tr><td>alpha</td><td>beta</td></tr><tr><td>1</td><td>2</td></tr></table>";
        let md = html_to_markdown(html);
        assert!(
            md.contains("alpha | beta"),
            "cells do not run together: {md}"
        );
        assert!(md.contains("1 | 2"));
    }

    #[test]
    fn markdown_conversion_of_plain_text_keeps_every_word() {
        // In HTML a source newline is formatting, not a line break, so the
        // degrade path (a non-HTML body mistaken for HTML) collapses lines
        // into one paragraph — words and punctuation all survive.
        let md = html_to_markdown("just plain text,\n  no tags at all");
        assert_eq!(md, "just plain text, no tags at all");
    }

    // --- markdown_to_text --------------------------------------------------------

    #[test]
    fn markdown_flattens_to_readable_text_for_excerpts() {
        let md = "## Steps\n\n- Use [the docs](https://docs.rs) first.\n\n```\nlet x = 1;\n```\n\nDone `now`.";
        let text = markdown_to_text(md);
        assert!(text.contains("Steps"), "{text}");
        assert!(!text.contains('#'), "heading markers stripped: {text}");
        assert!(
            text.contains("Use the docs first."),
            "link collapses to its text: {text}"
        );
        assert!(!text.contains("https://docs.rs"), "{text}");
        assert!(
            text.contains("let x = 1;"),
            "code content survives, fences do not: {text}"
        );
        assert!(!text.contains("```"), "{text}");
        assert!(
            text.contains("Done now."),
            "inline backticks removed: {text}"
        );
    }

    #[test]
    fn markdown_to_text_keeps_non_markdown_untouched() {
        assert_eq!(
            markdown_to_text("plain prose, nothing special"),
            "plain prose, nothing special"
        );
        // A literal bracket that is not link syntax stays put.
        assert_eq!(markdown_to_text("array[0] = 1"), "array[0] = 1");
    }
}
