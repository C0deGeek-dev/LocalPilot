//! Candidate-URL extraction from MCP tool results.
//!
//! Search-capable MCP servers share no result convention: some return plain
//! text with `URL:`-prefixed lines, some serialize JSON into text blocks
//! (spec-sanctioned — a tool with structured output SHOULD also serialize it
//! as text), some interleave non-result items, and future servers may use
//! `structuredContent` or `resource_link` items. This module parses them all
//! into an ordered, deduplicated list of candidate URLs, and classifies
//! `isError` results so a caller can back off on a rate limit instead of
//! treating it as an empty answer.
//!
//! Extraction order per result: `isError` (never parsed for URLs) →
//! `structuredContent` → each `text` content item (full-string JSON parse,
//! else plain-text scan) → `resource_link` item URIs. Image/audio/embedded
//! resource items are ignored.

use serde_json::Value;

/// Why a tool result carried `isError: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchCallError {
    /// The error text looks like a rate limit / quota rejection — the caller
    /// should back off or rotate providers rather than retry immediately.
    RateLimited(String),
    /// Any other execution error, with the server's explanation.
    Failed(String),
}

/// The parsed outcome of one search-tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchProposals {
    /// Candidate URLs in result order, exact-deduplicated.
    pub urls: Vec<String>,
    /// Present when the result carried `isError: true`; `urls` is then empty.
    pub error: Option<SearchCallError>,
}

/// Parse a raw `tools/call` result into candidate URLs.
///
/// Never fails: an unrecognized shape yields an empty proposal list (a
/// URL-less non-error result is an empty round, not a failure).
#[must_use]
pub fn extract_candidate_urls(result: &Value) -> SearchProposals {
    if result["isError"].as_bool() == Some(true) {
        let explanation = collect_text_items(result);
        return SearchProposals {
            urls: Vec::new(),
            error: Some(classify_error(&explanation)),
        };
    }

    let mut urls = Vec::new();
    if let Some(structured) = result.get("structuredContent") {
        collect_urls_from_value(structured, &mut urls);
    }
    if let Some(items) = result["content"].as_array() {
        for item in items {
            match item["type"].as_str() {
                Some("text") => {
                    let text = item["text"].as_str().unwrap_or_default();
                    // JSON-in-text is the dominant real-world shape; fall back
                    // to a plain-text scan when the block is not one JSON value.
                    if let Ok(value) = serde_json::from_str::<Value>(text) {
                        collect_urls_from_value(&value, &mut urls);
                    } else {
                        collect_urls_from_text(text, &mut urls);
                    }
                }
                Some("resource_link") => {
                    if let Some(uri) = item["uri"].as_str() {
                        if is_web_url(uri) {
                            urls.push(uri.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }
    dedup_preserving_order(&mut urls);
    SearchProposals { urls, error: None }
}

/// Markers that identify a rate-limit / quota rejection in error text. Kept
/// deliberately narrow: a false `Failed` merely skips the backoff hint, while
/// a false `RateLimited` would suppress a real error.
const RATE_LIMIT_MARKERS: &[&str] = &["rate limit", "rate-limit", "too many requests", "quota"];

fn classify_error(explanation: &str) -> SearchCallError {
    let lower = explanation.to_ascii_lowercase();
    if RATE_LIMIT_MARKERS.iter().any(|m| lower.contains(m)) || lower.contains("429") {
        SearchCallError::RateLimited(explanation.to_string())
    } else {
        SearchCallError::Failed(explanation.to_string())
    }
}

fn collect_text_items(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Walk a JSON value collecting web URLs from URL-shaped keys and any string
/// value that is itself a URL.
fn collect_urls_from_value(value: &Value, urls: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if is_web_url(text) {
                urls.push(text.clone());
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_urls_from_value(item, urls);
            }
        }
        Value::Object(map) => {
            // URL-shaped keys first so ranked result lists keep their order.
            for key in ["url", "link", "uri", "href"] {
                if let Some(Value::String(text)) = map.get(key) {
                    if is_web_url(text) {
                        urls.push(text.clone());
                    }
                }
            }
            for (key, nested) in map {
                if !["url", "link", "uri", "href"].contains(&key.as_str()) {
                    collect_urls_from_value(nested, urls);
                }
            }
        }
        _ => {}
    }
}

/// Scan plain text for URLs: bare `http(s)://` tokens wherever they appear
/// (covering `URL: https://…` lines and markdown `[text](https://…)` links
/// alike). A trailing `)`, `]`, or punctuation from surrounding prose is
/// trimmed.
fn collect_urls_from_text(text: &str, urls: &mut Vec<String>) {
    for scheme in ["https://", "http://"] {
        let mut rest = text;
        while let Some(pos) = rest.find(scheme) {
            let candidate = &rest[pos..];
            let end = candidate
                .find(|c: char| c.is_whitespace() || c == '"' || c == '<' || c == '>')
                .unwrap_or(candidate.len());
            let url = candidate[..end].trim_end_matches([')', ']', ',', '.', ';', '\'']);
            if url.len() > scheme.len() {
                urls.push(url.to_string());
            }
            rest = &candidate[end.min(candidate.len())..];
        }
    }
}

fn is_web_url(text: &str) -> bool {
    text.starts_with("https://") || text.starts_with("http://")
}

fn dedup_preserving_order(urls: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    urls.retain(|url| seen.insert(url.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn plain_text_url_lines_are_extracted() {
        // The DuckDuckGo-family shape: numbered entries with `URL:` lines.
        let result = json!({ "content": [{ "type": "text", "text":
            "Found 2 search results:\n\
             1. Three.js docs\n   URL: https://threejs.org/docs/\n   Summary: docs\n\
             2. Example\n   URL: https://example.com/animation\n   Summary: ex" }] });
        let proposals = extract_candidate_urls(&result);
        assert_eq!(
            proposals.urls,
            vec![
                "https://threejs.org/docs/".to_string(),
                "https://example.com/animation".to_string(),
            ]
        );
        assert!(proposals.error.is_none());
    }

    #[test]
    fn json_in_text_results_are_walked() {
        // The Brave-family shape: each content item is serialized JSON.
        let entry = json!({ "title": "T", "url": "https://docs.rs/tokio", "description": "d" });
        let result = json!({ "content": [
            { "type": "text", "text": entry.to_string() },
            { "type": "text", "text": json!({ "link": "https://crates.io/crates/tokio" }).to_string() },
        ] });
        let proposals = extract_candidate_urls(&result);
        assert_eq!(
            proposals.urls,
            vec![
                "https://docs.rs/tokio".to_string(),
                "https://crates.io/crates/tokio".to_string(),
            ]
        );
    }

    #[test]
    fn structured_content_and_resource_links_are_harvested() {
        let result = json!({
            "structuredContent": { "results": [{ "url": "https://a.example/one" }] },
            "content": [
                { "type": "resource_link", "uri": "https://b.example/two", "name": "two" },
                { "type": "image", "data": "...", "mimeType": "image/png" },
            ],
        });
        let proposals = extract_candidate_urls(&result);
        assert_eq!(
            proposals.urls,
            vec![
                "https://a.example/one".to_string(),
                "https://b.example/two".to_string(),
            ]
        );
    }

    #[test]
    fn markdown_links_and_prose_urls_are_scanned() {
        let result = json!({ "content": [{ "type": "text", "text":
            "See [the docs](https://threejs.org/manual/) and https://example.com/x, then stop." }] });
        let proposals = extract_candidate_urls(&result);
        assert_eq!(
            proposals.urls,
            vec![
                "https://threejs.org/manual/".to_string(),
                "https://example.com/x".to_string(),
            ]
        );
    }

    #[test]
    fn duplicate_urls_are_folded_in_order() {
        let result = json!({ "content": [{ "type": "text", "text":
            "https://a.example/1 https://b.example/2 https://a.example/1" }] });
        assert_eq!(
            extract_candidate_urls(&result).urls,
            vec![
                "https://a.example/1".to_string(),
                "https://b.example/2".to_string(),
            ]
        );
    }

    #[test]
    fn is_error_results_are_classified_not_parsed() {
        // A rate limit must be recognisable even when the text contains URLs.
        let result = json!({ "isError": true, "content": [{ "type": "text",
            "text": "429 Too Many Requests: retry at https://api.example/limits" }] });
        let proposals = extract_candidate_urls(&result);
        assert!(proposals.urls.is_empty(), "error text is never a proposal");
        assert!(matches!(
            proposals.error,
            Some(SearchCallError::RateLimited(_))
        ));

        let result = json!({ "isError": true, "content": [{ "type": "text",
            "text": "invalid query parameter" }] });
        assert!(matches!(
            extract_candidate_urls(&result).error,
            Some(SearchCallError::Failed(_))
        ));
    }

    #[test]
    fn url_less_prose_is_an_empty_round_not_an_error() {
        let result = json!({ "content": [{ "type": "text", "text": "No results found." }] });
        let proposals = extract_candidate_urls(&result);
        assert!(proposals.urls.is_empty());
        assert!(proposals.error.is_none());
    }
}
