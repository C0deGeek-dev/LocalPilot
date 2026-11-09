//! Checking the companion app bundle this binary embeds.
//!
//! The app is built in its own repository. A copy of its build output lives in
//! `client/`, and `client.pin` names the one build it must be: the source
//! revision and the SHA-256 of its `bundle.json`. The build script refuses to
//! compile unless the copy is exactly that build: clean source, every listed
//! file present with its size and digest, nothing else in the tree, safe
//! relative paths and an allowed content type for each.
//!
//! Included by both `build.rs` (which runs it) and the crate's tests (which
//! exercise it), so the rule has exactly one definition.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The build a pin names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pin {
    pub(crate) revision: String,
    pub(crate) bundle_sha256: String,
}

/// One file the gateway may serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientFile {
    /// Relative path, `/`-separated.
    pub(crate) path: String,
    pub(crate) content_type: &'static str,
}

/// A bundle that passed every check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verified {
    pub(crate) revision: String,
    pub(crate) version: String,
    pub(crate) protocol_min: u64,
    pub(crate) protocol_max: u64,
    pub(crate) capabilities: Vec<String>,
    pub(crate) files: Vec<ClientFile>,
}

/// The content types a bundle may contain, by file extension.
const CONTENT_TYPES: [(&str, &str); 6] = [
    ("html", "text/html; charset=utf-8"),
    ("js", "application/javascript; charset=utf-8"),
    ("css", "text/css; charset=utf-8"),
    ("webmanifest", "application/manifest+json"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
];

/// The metadata file inside the bundle; never served.
pub(crate) const MANIFEST: &str = "bundle.json";

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Read `client.pin`: `revision <40 hex>` and `bundle-sha256 <64 hex>` lines;
/// blank lines and `#` comments are ignored.
pub(crate) fn parse_pin(text: &str) -> Result<Pin, String> {
    let (mut revision, mut digest) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.split_once(' ') {
            Some(("revision", value)) if revision.is_none() => revision = Some(value.trim()),
            Some(("bundle-sha256", value)) if digest.is_none() => digest = Some(value.trim()),
            _ => return Err(format!("unexpected line in client.pin: {line}")),
        }
    }
    match (revision, digest) {
        (Some(revision), Some(digest)) if is_hex(revision, 40) && is_hex(digest, 64) => Ok(Pin {
            revision: revision.to_string(),
            bundle_sha256: digest.to_string(),
        }),
        _ => Err("client.pin needs a 40-hex `revision` and a 64-hex `bundle-sha256`".to_string()),
    }
}

/// A relative path of plain segments: letters, digits, `_`, `-` and inner dots,
/// so no empty segment, no `.` or `..`, no leading or trailing separator.
pub(crate) fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.ends_with('.')
                && !segment.contains("..")
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
        })
}

fn content_type(path: &str) -> Option<&'static str> {
    let extension = path.rsplit_once('.')?.1;
    CONTENT_TYPES
        .iter()
        .find(|(known, _)| *known == extension)
        .map(|(_, content_type)| *content_type)
}

/// Every regular file under `root`, as sorted `/`-separated relative paths.
fn walk(root: &Path, prefix: &str, out: &mut Vec<String>) -> Result<(), String> {
    let dir = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let entries =
        std::fs::read_dir(&dir).map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list {}: {e}", dir.display()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("a file name under {} is not UTF-8", dir.display()))?;
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry
            .file_type()
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        if kind.is_dir() {
            walk(root, &path, out)?;
        } else if kind.is_file() {
            out.push(path);
        } else {
            return Err(format!("{path} is not a regular file"));
        }
    }
    Ok(())
}

/// Check the bundle in `dir` against `pin`.
pub(crate) fn verify(dir: &Path, pin: &Pin) -> Result<Verified, String> {
    let raw = std::fs::read(dir.join(MANIFEST))
        .map_err(|e| format!("the pinned companion app is missing ({MANIFEST}: {e})"))?;
    let digest = hex(&Sha256::digest(&raw));
    if digest != pin.bundle_sha256 {
        return Err(format!(
            "{MANIFEST} has SHA-256 {digest}, but client.pin names {}",
            pin.bundle_sha256
        ));
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|e| format!("{MANIFEST} is not JSON: {e}"))?;
    let field = |name: &str| {
        manifest
            .get(name)
            .ok_or_else(|| format!("{MANIFEST} has no `{name}`"))
    };
    if field("schema_version")?.as_u64() != Some(1) {
        return Err(format!("{MANIFEST} is not schema version 1"));
    }
    if field("source_revision")?.as_str() != Some(pin.revision.as_str()) {
        return Err(format!(
            "{MANIFEST} was built from another revision than client.pin names"
        ));
    }
    if field("source_dirty")?.as_bool() != Some(false) {
        return Err(format!(
            "{MANIFEST} was built from uncommitted source; pin a release build"
        ));
    }
    let version = field("version")?
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{MANIFEST} has no version"))?
        .to_string();
    let protocol = field("protocol")?;
    let bound = |name: &str| {
        protocol
            .get(name)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| format!("{MANIFEST} has no protocol `{name}`"))
    };
    let (protocol_min, protocol_max) = (bound("min")?, bound("max")?);
    if protocol_min > protocol_max || protocol_max > u64::from(u32::MAX) {
        return Err(format!("{MANIFEST} has an invalid protocol range"));
    }
    let capabilities = protocol
        .get("required_capabilities")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| format!("{MANIFEST} has no required capabilities"))?
        .iter()
        .map(|c| {
            c.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{MANIFEST} lists a capability that is not text"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let listed = field("files")?
        .as_array()
        .ok_or_else(|| format!("{MANIFEST} `files` is not a list"))?;
    let mut files = Vec::with_capacity(listed.len());
    let mut previous: Option<&str> = None;
    for entry in listed {
        let text = |name: &str| entry.get(name).and_then(serde_json::Value::as_str);
        let path = text("path").ok_or_else(|| format!("a file in {MANIFEST} has no path"))?;
        if !safe_path(path) || path == MANIFEST {
            return Err(format!("unsafe path in {MANIFEST}: {path}"));
        }
        if previous.is_some_and(|before| before >= path) {
            return Err(format!(
                "{MANIFEST} paths are not sorted and unique at {path}"
            ));
        }
        previous = Some(path);
        let expected = content_type(path)
            .ok_or_else(|| format!("{path} has a type the gateway does not serve"))?;
        if text("content_type") != Some(expected) {
            return Err(format!("{path} does not declare {expected}"));
        }
        let bytes = std::fs::read(dir.join(path)).map_err(|e| format!("{path}: {e}"))?;
        let size = entry.get("bytes").and_then(serde_json::Value::as_u64);
        if size != Some(bytes.len() as u64)
            || text("sha256") != Some(hex(&Sha256::digest(&bytes)).as_str())
        {
            return Err(format!(
                "{path} does not match its size or digest in {MANIFEST}"
            ));
        }
        files.push(ClientFile {
            path: path.to_string(),
            content_type: expected,
        });
    }
    if !files.iter().any(|f| f.path == "index.html") {
        return Err(format!("{MANIFEST} lists no index.html"));
    }
    let mut present = Vec::new();
    walk(dir, "", &mut present)?;
    let present: BTreeSet<String> = present.into_iter().filter(|p| p != MANIFEST).collect();
    let expected: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
    if present != expected {
        let extra: Vec<_> = present.difference(&expected).collect();
        return Err(format!("files not listed in {MANIFEST}: {extra:?}"));
    }
    Ok(Verified {
        revision: pin.revision.clone(),
        version,
        protocol_min,
        protocol_max,
        capabilities,
        files,
    })
}

/// Rust source embedding a verified bundle that lives in `dir`.
pub(crate) fn render(dir: &Path, bundle: &Verified) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "/// The companion app's source revision.");
    let _ = writeln!(
        out,
        "pub(crate) const CLIENT_REVISION: &str = {:?};",
        bundle.revision
    );
    let _ = writeln!(out, "/// The companion app's version.");
    let _ = writeln!(
        out,
        "pub(crate) const CLIENT_VERSION: &str = {:?};",
        bundle.version
    );
    let _ = writeln!(out, "/// The RPC protocol range the app speaks.");
    let _ = writeln!(
        out,
        "pub(crate) const CLIENT_PROTOCOL: (u32, u32) = ({}, {});",
        bundle.protocol_min, bundle.protocol_max
    );
    let _ = writeln!(out, "/// The server capabilities the app requires.");
    let _ = writeln!(
        out,
        "pub(crate) const CLIENT_CAPABILITIES: &[&str] = &{:?};",
        bundle.capabilities
    );
    let _ = writeln!(
        out,
        "/// (path, content type, bytes) for every served file."
    );
    let _ = writeln!(
        out,
        "pub(crate) static CLIENT_FILES: &[(&str, &str, &[u8])] = &["
    );
    for file in &bundle.files {
        let source = dir.join(&file.path);
        let _ = writeln!(
            out,
            "    ({:?}, {:?}, include_bytes!({:?})),",
            file.path,
            file.content_type,
            source.display().to_string()
        );
    }
    let _ = writeln!(out, "];");
    out
}
