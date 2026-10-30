//! A page that loads a library from a CDN must name an exact version.
//!
//! After a step, the files it changed are scanned for URLs on the common
//! npm-mirroring CDNs. A URL without a full version (`@latest`, a range, a bare
//! major, or no version at all) is a finding the `cdn_version_pinned` rule feeds
//! back to the model with the procedure to fix it. Nothing here runs for a
//! project that has no such URL, and nothing touches the network: the scan reads
//! text only, so it can say a version is missing, not that the one written
//! exists.

use std::path::Path;

/// Files a browser page is made of. Anything else is never read.
const WEB_EXTENSIONS: &[&str] = &[
    "html", "htm", "js", "mjs", "cjs", "jsx", "ts", "tsx", "vue", "svelte", "astro",
];

/// Larger files are vendored or generated, not written by a step.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Findings reported to the model; the rest are counted.
const MAX_REPORTED: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Host {
    /// `jsdelivr.net/npm/<pkg>@<version>/...`
    Npm,
    /// `esm.sh/[v<build>/]<pkg>@<version>/...`
    Esm,
    /// `cdn.skypack.dev/<pkg>@<version>/...`
    Skypack,
    /// `cdnjs.cloudflare.com/ajax/libs/<lib>/<version>/<file>`
    Cdnjs,
}

const HOSTS: &[(&str, Host)] = &[
    ("cdn.jsdelivr.net/npm/", Host::Npm),
    ("unpkg.com/", Host::Npm),
    ("esm.sh/", Host::Esm),
    ("cdn.skypack.dev/", Host::Skypack),
    ("cdnjs.cloudflare.com/ajax/libs/", Host::Cdnjs),
];

/// A CDN URL that does not name an exact version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unpinned {
    pub(crate) package: String,
    pub(crate) url: String,
}

/// Findings in the changed files, one `path: url` line each, then the procedure.
/// Empty when every CDN URL names an exact version (or there is none).
pub(crate) fn unpinned_in_files(root: &Path, paths: &[String]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut packages: Vec<String> = Vec::new();
    for path in paths {
        let is_web = Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                WEB_EXTENSIONS
                    .iter()
                    .any(|web| extension.eq_ignore_ascii_case(web))
            });
        if !is_web {
            continue;
        }
        let full = root.join(path);
        let Ok(meta) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        for finding in scan(&text) {
            if !packages.contains(&finding.package) {
                packages.push(finding.package.clone());
            }
            lines.push(format!("{path}: {}", finding.url));
        }
    }
    if lines.is_empty() {
        return lines;
    }
    let hidden = lines.len().saturating_sub(MAX_REPORTED);
    lines.truncate(MAX_REPORTED);
    if hidden > 0 {
        lines.push(format!("... and {hidden} more"));
    }
    let example = packages.first().map_or("<package>", String::as_str);
    lines.push(format!(
        "Resolve each exact version before writing it: `npm view {example} version` (or \
         https://registry.npmjs.org/{example}/latest), list that version's files at \
         https://unpkg.com/{example}@<version>/?meta, check that every import URL returns 200, then \
         write the full version (x.y.z) in every CDN URL and import map. Use one CDN for a library \
         and its addons. Never `@latest`, a range, or a bare major."
    ));
    lines
}

/// Every CDN URL in `text` that does not name an exact version.
pub(crate) fn scan(text: &str) -> Vec<Unpinned> {
    let mut found: Vec<Unpinned> = Vec::new();
    for (marker, host) in HOSTS {
        let mut from = 0;
        while let Some(offset) = text[from..].find(marker) {
            let start = from + offset;
            from = start + marker.len();
            // `myunpkg.com/` is another host.
            if text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            {
                continue;
            }
            let rest = &text[from..];
            let end = rest.find(is_terminator).unwrap_or(rest.len());
            let target = &rest[..end];
            if let Some(package) = unpinned_package(*host, target) {
                let url = shorten(&format!("{marker}{target}"));
                let finding = Unpinned { package, url };
                if !found.contains(&finding) {
                    found.push(finding);
                }
            }
        }
    }
    found
}

fn is_terminator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '\\' | '?' | '#' | ','
        )
}

fn shorten(url: &str) -> String {
    if url.chars().count() <= 100 {
        return url.to_string();
    }
    let head: String = url.chars().take(97).collect();
    format!("{head}...")
}

/// The package name when `target` (the URL path after the host marker) loads it
/// without an exact version; `None` when pinned, or not something to judge.
fn unpinned_package(host: Host, target: &str) -> Option<String> {
    // A URL built at runtime is not text to judge.
    if target.is_empty() || target.contains("${") || target.contains("{{") || target.contains('%') {
        return None;
    }
    match host {
        Host::Cdnjs => {
            let mut segments = target.split('/');
            let library = segments.next().filter(|name| !name.is_empty())?;
            // A trailing slash or file name in the version slot is not a version.
            let version = segments.next();
            let pinned = segments.next().is_some()
                && version.is_some_and(|version| {
                    let mut chars = version.chars();
                    match chars.next() {
                        Some(first) if first.is_ascii_digit() => true,
                        Some('r' | 'v') => chars.next().is_some_and(|c| c.is_ascii_digit()),
                        _ => false,
                    }
                });
            (!pinned).then(|| library.to_string())
        }
        Host::Npm | Host::Esm | Host::Skypack => {
            let mut path = target;
            if host == Host::Esm {
                path = path.trim_start_matches('*');
                // `esm.sh/v135/react@18.2.0` and `esm.sh/stable/react`.
                if let Some((first, after)) = path.split_once('/') {
                    let build = first == "stable"
                        || (first.len() > 1
                            && first.starts_with('v')
                            && first[1..].chars().all(|c| c.is_ascii_digit()));
                    if build {
                        path = after;
                    }
                }
                if path.starts_with("gh/") || path.starts_with("jsr/") {
                    return None;
                }
            }
            if host == Host::Skypack && (path.starts_with("pin/") || path.starts_with("-/")) {
                return None;
            }
            let (name, version) = split_spec(path)?;
            if !is_package_name(name) {
                return None;
            }
            (!version.is_some_and(is_exact_version)).then(|| name.to_string())
        }
    }
}

/// `pkg@1.2.3/file`, `@scope/pkg@1.2.3/file`, `pkg/file` as (name, version).
fn split_spec(path: &str) -> Option<(&str, Option<&str>)> {
    let spec_end = if path.starts_with('@') {
        let scope_end = path.find('/')?;
        path[scope_end + 1..]
            .find('/')
            .map_or(path.len(), |index| scope_end + 1 + index)
    } else {
        path.find('/').unwrap_or(path.len())
    };
    let spec = &path[..spec_end];
    match spec[1.min(spec.len())..].rfind('@') {
        Some(index) => {
            let at = index + 1.min(spec.len());
            Some((&spec[..at], Some(&spec[at + 1..])))
        }
        None => Some((spec, None)),
    }
}

fn is_package_name(name: &str) -> bool {
    let bare = name.strip_prefix('@').unwrap_or(name);
    !bare.is_empty()
        && bare
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '/'))
}

/// `1.2.3`, optionally with a pre-release or build part (`1.2.3-rc.1`).
fn is_exact_version(version: &str) -> bool {
    let (core, extra) = match version.find(['-', '+']) {
        Some(index) => (&version[..index], Some(&version[index + 1..])),
        None => (version, None),
    };
    let numbers: Vec<&str> = core.split('.').collect();
    numbers.len() == 3
        && numbers
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
        && extra.is_none_or(|extra| {
            !extra.is_empty()
                && extra
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packages(text: &str) -> Vec<String> {
        scan(text)
            .into_iter()
            .map(|finding| finding.package)
            .collect()
    }

    #[test]
    fn a_url_without_an_exact_version_is_a_finding() {
        for (text, package) in [
            (
                r#"<script src="https://cdn.jsdelivr.net/npm/three/build/three.module.js">"#,
                "three",
            ),
            (
                r#"import * as T from "https://unpkg.com/three@latest/build/three.module.js";"#,
                "three",
            ),
            (
                r#"<script src='https://unpkg.com/lodash@4/lodash.js'></script>"#,
                "lodash",
            ),
            (
                r#"from "https://unpkg.com/lodash@^4.17.0/lodash.js""#,
                "lodash",
            ),
            (
                r#"from "https://unpkg.com/lodash@4.17/lodash.js""#,
                "lodash",
            ),
            (
                r#"{"imports": {"x": "https://cdn.jsdelivr.net/npm/@scope/pkg@2/dist/x.js"}}"#,
                "@scope/pkg",
            ),
            (r#"import r from "https://esm.sh/react";"#, "react"),
            (r#"import r from "https://esm.sh/v135/react@18";"#, "react"),
            (r#"import r from "https://cdn.skypack.dev/react";"#, "react"),
            (
                r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/jquery/latest/jquery.min.js">"#,
                "jquery",
            ),
            (
                r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/jquery/jquery.min.js">"#,
                "jquery",
            ),
        ] {
            assert_eq!(packages(text), vec![package.to_string()], "{text}");
        }
    }

    #[test]
    fn an_exact_version_is_not_a_finding() {
        for text in [
            r#"<script src="https://cdn.jsdelivr.net/npm/three@0.186.1/build/three.module.js">"#,
            r#"import T from "https://unpkg.com/three@0.186.1/build/three.module.js";"#,
            r#"from "https://unpkg.com/@scope/pkg@1.2.3/dist/x.js""#,
            r#"from "https://unpkg.com/vue@3.5.0-rc.1/dist/vue.js""#,
            r#"import r from "https://esm.sh/v135/react@18.2.0";"#,
            r#"import r from "https://esm.sh/*react@18.2.0";"#,
            r#"import r from "https://cdn.skypack.dev/pin/react@v18.2.0-abcdef/mode=imports/optimized/react.js";"#,
            r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/three.js/0.186.1/three.module.min.js">"#,
            r#"<script src="https://cdnjs.cloudflare.com/ajax/libs/three.js/r128/three.min.js">"#,
        ] {
            assert_eq!(packages(text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn what_is_not_a_cdn_url_to_judge_is_left_alone() {
        for text in [
            // Built at run time.
            r#"const u = `https://unpkg.com/${name}@${version}/index.js`;"#,
            r#"<script src="https://unpkg.com/{{ pkg }}/x.js">"#,
            // The metadata listing and another host with the same ending.
            r#"fetch("https://unpkg.com/three@0.186.1/?meta")"#,
            r#"fetch("https://data.jsdelivr.com/v1/package/npm/three@0.186.1/flat")"#,
            r#"fetch("https://registry.npmjs.org/three/latest")"#,
            r#"<a href="https://myunpkg.com/three">"#,
            // A local file, and a mention without a path.
            r#"<script src="./vendor/three.module.js">"#,
            "see unpkg.com for details",
        ] {
            assert_eq!(packages(text), Vec::<String>::new(), "{text}");
        }
    }

    #[test]
    fn only_the_changed_web_files_are_read_and_the_feedback_names_the_procedure() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("index.html"),
            "<script type=\"module\" src=\"https://unpkg.com/three@latest/build/three.module.js\"></script>\n",
        )
        .unwrap();
        std::fs::write(
            root.join("notes.md"),
            "https://unpkg.com/three@latest/build/three.module.js\n",
        )
        .unwrap();
        std::fs::write(
            root.join("pinned.js"),
            "import 'https://unpkg.com/three@0.186.1/build/three.module.js';\n",
        )
        .unwrap();
        let none = unpinned_in_files(
            root,
            &[
                "notes.md".to_string(),
                "pinned.js".to_string(),
                "gone.html".to_string(),
            ],
        );
        assert!(none.is_empty(), "{none:?}");
        let lines = unpinned_in_files(root, &["index.html".to_string()]);
        assert_eq!(
            lines[0],
            "index.html: unpkg.com/three@latest/build/three.module.js"
        );
        let advice = lines.last().unwrap();
        assert!(advice.contains("npm view three version"), "{advice}");
        assert!(advice.contains("/?meta"), "{advice}");
        assert!(advice.contains("Never `@latest`"), "{advice}");
    }

    #[test]
    fn a_long_list_is_cut_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let mut body = String::new();
        for index in 0..12 {
            body.push_str(&format!("import 'https://unpkg.com/pkg{index}/x.js';\n"));
        }
        std::fs::write(dir.path().join("a.js"), body).unwrap();
        let lines = unpinned_in_files(dir.path(), &["a.js".to_string()]);
        assert_eq!(lines.len(), MAX_REPORTED + 2);
        assert_eq!(lines[MAX_REPORTED], "... and 4 more");
    }
}
