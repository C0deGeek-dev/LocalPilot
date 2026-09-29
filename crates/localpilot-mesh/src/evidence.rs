//! Read-only evidence over a working tree: find text, pin a range of lines
//! to a hash, and later check that the pinned lines are still there.
//!
//! This is a LocalPilot service for the participants of a pair session, not
//! part of the mailbox protocol. It reads files and nothing else: it never
//! writes, never follows a link, and never runs a command.
//!
//! - [`locate`] walks the tree (`.gitignore` and `.pairignore` honoured,
//!   links never followed) and returns bounded hits.
//! - [`anchor`] hashes lines `start..=end` of one file as they are now.
//! - [`verify`] checks an anchor against the file as it is now: `ok`,
//!   `moved` (exactly one other place in the same file), `ambiguous` (more
//!   than one), `stale` (the lines or the file are gone) or `unknown` (the
//!   file could not be checked).
//!
//! An anchor's hash is taken over the file's current bytes with CRLF read as
//! LF: SHA-256 of lines `start..=end` joined by `\n`, without a final
//! newline. Anyone can compute one, so a hash that verifies proves only that
//! the file now holds lines with that hash, not who took it or when. Proof
//! that the lines are the ones a reader was shown comes from the caller
//! keeping the anchor this module returned when it read them.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::layout::MAILBOX_DIR;
use crate::tree;

/// The limits of one request. A request that reaches one stops there and
/// says which, rather than reporting a partial answer as complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Files read by one `locate`.
    pub files: usize,
    /// The largest file read, in bytes; a larger file is skipped and named.
    pub file_bytes: u64,
    /// Bytes read by one `locate`, over all files.
    pub total_bytes: u64,
    /// Wall time of one `locate`.
    pub wall: Duration,
    /// Hits returned by one `locate`.
    pub hits: usize,
    /// Bytes of a hit's line returned.
    pub snippet: usize,
    /// The longest pattern accepted, in bytes.
    pub pattern: usize,
    /// The compiled regex's size limit, in bytes.
    pub regex_size: usize,
    /// Anchors checked by one `verify`.
    pub anchors: usize,
    /// Bytes hashed by one `verify` while looking for moved lines, over all
    /// its anchors.
    pub relocation_bytes: u64,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            files: 2_000,
            file_bytes: 1 << 20,
            total_bytes: 64 << 20,
            wall: Duration::from_secs(10),
            hits: 200,
            snippet: 256,
            pattern: 1_024,
            regex_size: 1 << 20,
            anchors: 100,
            relocation_bytes: 64 << 20,
        }
    }
}

/// Why a request was refused before it read anything.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceError {
    /// The path is absolute, climbs out of the tree, or is empty.
    #[error("{0}: not a path inside the tree")]
    OutsideTree(String),
    /// The path, or a directory on the way to it, is a link.
    #[error("{0}: a link, which evidence never follows")]
    Link(String),
    /// The name looks like it holds secrets.
    #[error("{0}: looks like a secrets file, which evidence does not read")]
    SecretLike(String),
    /// The path is inside the mailbox or `.git`.
    #[error("{0}: mailbox and Git metadata are not evidence")]
    Metadata(String),
    /// The file is larger than the per-file bound.
    #[error("{0}: larger than {1} bytes")]
    TooLarge(String, u64),
    /// The file is missing or not a regular file.
    #[error("{0}: no such file")]
    Missing(String),
    /// The file, or a directory on the way to it, changed while it was read.
    #[error("{0}: changed while it was read")]
    Changed(String),
    /// The range is empty, reversed or past the end of the file.
    #[error("{path}: lines {start}-{end} are not in the file ({lines} lines)")]
    BadRange {
        path: String,
        start: usize,
        end: usize,
        lines: usize,
    },
    /// The pattern is too long or does not compile.
    #[error("pattern: {0}")]
    Pattern(String),
    /// Any other read failure.
    #[error("{0}: {1}")]
    Io(String, String),
}

/// A range of lines pinned to their hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// `/`-separated, relative to the tree's root.
    pub path: String,
    /// First line, from 1.
    pub start: usize,
    /// Last line, inclusive.
    pub end: usize,
    /// SHA-256 of the lines, lowercase hex.
    pub sha: String,
}

/// An anchor as `anchor` returns it: the record, and the lines it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Anchored {
    #[serde(flatten)]
    pub anchor: Anchor,
    /// The lines covered, lossily decoded.
    pub text: String,
}

/// One `locate` hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub anchor: Anchor,
    /// The line, lossily decoded and cut to the snippet bound.
    pub snippet: String,
}

/// What `locate` found.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Located {
    pub hits: Vec<Hit>,
    /// Files read.
    pub files_read: usize,
    /// The bound that stopped the walk, if one did.
    pub truncated: Option<&'static str>,
    /// Files that changed while being read; they are not searched.
    pub changed_during_read: Vec<String>,
    /// Files skipped for their size.
    pub too_large: Vec<String>,
    /// Files and directories that could not be read, with why.
    pub unreadable: Vec<String>,
    /// Files whose names are not valid Unicode, so no anchor could address
    /// them; they are not searched. Shown lossily.
    pub unaddressable: Vec<String>,
    /// Whether every file the request covers was searched: nothing
    /// truncated, skipped for size, changed, unreadable or unaddressable.
    pub complete: bool,
}

/// An anchor checked against the tree as it is now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum Check {
    /// The same lines are where the anchor says.
    Ok,
    /// The lines are now exactly once elsewhere in the same file.
    Moved { start: usize, end: usize },
    /// The lines are now in more than one place; none is chosen.
    Ambiguous { places: usize },
    /// The lines are gone, or the file cannot be checked.
    Stale { why: String },
    /// The lines are not where the anchor says, and looking for them
    /// elsewhere would pass the request's work bound.
    Unknown { why: String },
}

/// Lines `start..=end` (from 1) of `bytes`, CRLF read as LF.
fn split_lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut text = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        text.push(bytes[i]);
        i += 1;
    }
    let mut lines: Vec<Vec<u8>> = text.split(|b| *b == b'\n').map(<[u8]>::to_vec).collect();
    if text.last() == Some(&b'\n') || text.is_empty() {
        lines.pop();
    }
    lines
}

fn hash_lines(lines: &[Vec<u8>]) -> String {
    let mut h = Sha256::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            h.update(b"\n");
        }
        h.update(l);
    }
    tree::hex(&h.finalize())
}

/// Whether a name is `.git` or the mailbox directory, in any case, and with
/// the trailing dots and spaces Windows ignores.
fn is_reserved(name: &std::ffi::OsStr) -> bool {
    let n = name.to_string_lossy();
    let n = if cfg!(windows) {
        n.trim_end_matches(['.', ' '])
    } else {
        &n
    };
    n.eq_ignore_ascii_case(".git") || n.eq_ignore_ascii_case(MAILBOX_DIR)
}

/// The deepest existing ancestor of `path` (itself included), canonical.
fn canonical_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|a| fs::symlink_metadata(a).is_ok())
        .and_then(|a| dunce::canonicalize(a).ok())
}

/// The file at `rel` under `root`, checked for every refusal: inside the
/// tree, no link on the way, not metadata, not secret-like. The checks run on
/// the name as given and again on the file system's own names for it, so a
/// case variant or a Windows short name (`ENV~1`) cannot reach what the
/// given name would be refused for.
///
/// # Errors
/// The refusal.
pub fn checked_path(root: &Path, rel: &str) -> Result<PathBuf, EvidenceError> {
    let rel_path = Path::new(rel);
    let mut parts = Vec::new();
    for c in rel_path.components() {
        match c {
            Component::Normal(p) => parts.push(p.to_owned()),
            Component::CurDir => {}
            _ => return Err(EvidenceError::OutsideTree(rel.to_owned())),
        }
    }
    if parts.is_empty() {
        return Err(EvidenceError::OutsideTree(rel.to_owned()));
    }
    if parts.iter().any(|p| is_reserved(p)) {
        return Err(EvidenceError::Metadata(rel.to_owned()));
    }
    if localpilot_sandbox::is_secret_like(rel_path) {
        return Err(EvidenceError::SecretLike(rel.to_owned()));
    }
    let mut path = root.to_path_buf();
    for p in &parts {
        path.push(p);
        if tree::is_link(&path) {
            return Err(EvidenceError::Link(rel.to_owned()));
        }
    }
    // The same checks on the names the file system resolves the path to.
    let (Some(real_root), Some(real)) = (dunce::canonicalize(root).ok(), canonical_existing(&path))
    else {
        return Err(EvidenceError::Io(
            rel.to_owned(),
            "cannot resolve the tree".into(),
        ));
    };
    let Ok(inside) = real.strip_prefix(&real_root) else {
        return Err(EvidenceError::OutsideTree(rel.to_owned()));
    };
    if inside.components().any(|c| is_reserved(c.as_os_str())) {
        return Err(EvidenceError::Metadata(rel.to_owned()));
    }
    if real != real_root && localpilot_sandbox::is_secret_like(&real) {
        return Err(EvidenceError::SecretLike(rel.to_owned()));
    }
    Ok(path)
}

fn stamp(meta: &fs::Metadata) -> (u64, Option<SystemTime>) {
    (meta.len(), meta.modified().ok())
}

/// Whether an opened handle is the file `before` described. On Unix the
/// device and inode must match, so a link swapped in after the check is
/// caught; on Windows the handle is opened on any reparse point itself and
/// refused below.
#[cfg(unix)]
fn same_file(before: &fs::Metadata, opened: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    before.dev() == opened.dev() && before.ino() == opened.ino()
}

#[cfg(not(unix))]
fn same_file(_: &fs::Metadata, _: &fs::Metadata) -> bool {
    true
}

#[cfg(windows)]
fn is_reparse(meta: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse(_: &fs::Metadata) -> bool {
    false
}

/// Read the regular file at `path` (already through [`checked_path`]),
/// reading at most `limit` bytes from the handle itself.
///
/// The file is never read through a link: a link found before opening is
/// refused, and a link swapped in before the open is caught on the handle
/// (a different inode on Unix; on Windows the handle is opened on the reparse
/// point itself). A file that changes while it is read, or a directory on the
/// way that became a link, is reported as changed.
fn read_file(root: &Path, rel: &str, limit: u64) -> Result<Vec<u8>, EvidenceError> {
    let path = checked_path(root, rel)?;
    let io = |e: std::io::Error| EvidenceError::Io(rel.to_owned(), e.to_string());
    let before = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(EvidenceError::Missing(rel.to_owned()))
        }
        Err(e) => return Err(io(e)),
    };
    if before.file_type().is_symlink() || is_reparse(&before) {
        return Err(EvidenceError::Link(rel.to_owned()));
    }
    if !before.is_file() {
        return Err(EvidenceError::Missing(rel.to_owned()));
    }
    if before.len() > limit {
        return Err(EvidenceError::TooLarge(rel.to_owned(), limit));
    }
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = match opts.open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(EvidenceError::Changed(rel.to_owned()))
        }
        Err(e) => return Err(io(e)),
    };
    let opened = file.metadata().map_err(io)?;
    if is_reparse(&opened) || !opened.is_file() || !same_file(&before, &opened) {
        return Err(EvidenceError::Changed(rel.to_owned()));
    }
    let mut bytes = Vec::new();
    (&file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(io)?;
    if bytes.len() as u64 > limit {
        return Err(EvidenceError::TooLarge(rel.to_owned(), limit));
    }
    let after = file.metadata().map_err(io)?;
    if stamp(&after) != stamp(&opened) || bytes.len() as u64 != opened.len() {
        return Err(EvidenceError::Changed(rel.to_owned()));
    }
    // A directory on the way that became a link during the read.
    match checked_path(root, rel) {
        Ok(_) => Ok(bytes),
        Err(EvidenceError::Link(_)) => Err(EvidenceError::Changed(rel.to_owned())),
        Err(e) => Err(e),
    }
}

/// Pin lines `start..=end` of `rel` to their hash, as they are now.
///
/// # Errors
/// A refused path, a missing or too-large file, or a range not in the file.
pub fn anchor(
    root: &Path,
    rel: &str,
    start: usize,
    end: usize,
    bounds: &Bounds,
) -> Result<Anchored, EvidenceError> {
    let lines = split_lines(&read_file(root, rel, bounds.file_bytes)?);
    if start == 0 || end < start || end > lines.len() {
        return Err(EvidenceError::BadRange {
            path: rel.to_owned(),
            start,
            end,
            lines: lines.len(),
        });
    }
    let window = &lines[start - 1..end];
    let text = window
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Anchored {
        anchor: Anchor {
            path: normal_rel(rel),
            start,
            end,
            sha: hash_lines(window),
        },
        text,
    })
}

/// Pin every line of `rel` to one hash, as it is now: the same bounded,
/// no-follow read as [`anchor`], counting lines the same way.
///
/// # Errors
/// A refused path, a missing or too-large file, or a file with no lines.
pub fn anchor_file(root: &Path, rel: &str, bounds: &Bounds) -> Result<Anchored, EvidenceError> {
    let lines = split_lines(&read_file(root, rel, bounds.file_bytes)?);
    if lines.is_empty() {
        return Err(EvidenceError::BadRange {
            path: rel.to_owned(),
            start: 1,
            end: 1,
            lines: 0,
        });
    }
    let text = lines
        .iter()
        .map(|l| String::from_utf8_lossy(l).into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Anchored {
        anchor: Anchor {
            path: normal_rel(rel),
            start: 1,
            end: lines.len(),
            sha: hash_lines(&lines),
        },
        text,
    })
}

/// `rel` with `/` separators and no `./`.
fn normal_rel(rel: &str) -> String {
    Path::new(rel)
        .components()
        .filter_map(|c| match c {
            Component::Normal(p) => Some(p.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Whether `sha` is a well-formed anchor hash.
#[must_use]
pub fn is_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Check `a` against the file as it is now. `spent` counts the bytes hashed
/// while looking for moved lines, across a request's anchors; once looking
/// would pass `bounds.relocation_bytes`, the answer is `unknown`.
#[must_use]
pub fn verify(root: &Path, a: &Anchor, bounds: &Bounds, spent: &mut u64) -> Check {
    let stale = |why: &str| Check::Stale {
        why: why.to_owned(),
    };
    if !is_sha(&a.sha) || a.start == 0 || a.end < a.start {
        return stale("invalid anchor hash");
    }
    let bytes = match read_file(root, &a.path, bounds.file_bytes) {
        Ok(b) => b,
        Err(EvidenceError::Missing(_)) => return stale("the file is gone"),
        // Not an address this service could have issued an anchor for.
        Err(
            e @ (EvidenceError::OutsideTree(_)
            | EvidenceError::Metadata(_)
            | EvidenceError::SecretLike(_)),
        ) => return stale(&e.to_string()),
        // The file is there but could not be checked: a link, too large,
        // changed while read, or unreadable. That says nothing about the
        // lines.
        Err(e) => return Check::Unknown { why: e.to_string() },
    };
    let lines = split_lines(&bytes);
    let width = a.end - a.start + 1;
    if a.end <= lines.len() && hash_lines(&lines[a.start - 1..a.end]) == a.sha {
        return Check::Ok;
    }
    if width > lines.len() {
        return stale("the lines are gone");
    }
    // The bytes every candidate window would hash, known before hashing any.
    let mut prefix = Vec::with_capacity(lines.len() + 1);
    prefix.push(0_u64);
    for l in &lines {
        prefix.push(prefix[prefix.len() - 1] + l.len() as u64 + 1);
    }
    let cost: u64 = (0..=lines.len() - width)
        .map(|i| prefix[i + width] - prefix[i])
        .sum();
    if spent.saturating_add(cost) > bounds.relocation_bytes {
        return Check::Unknown {
            why: "the lines are not where the anchor says, and looking for them elsewhere passes the work bound".into(),
        };
    }
    *spent += cost;
    let places: Vec<usize> = (0..=lines.len() - width)
        .filter(|&i| i + 1 != a.start && hash_lines(&lines[i..i + width]) == a.sha)
        .collect();
    match places.as_slice() {
        [] => stale("the lines are gone"),
        [i] => Check::Moved {
            start: i + 1,
            end: i + width,
        },
        more => Check::Ambiguous { places: more.len() },
    }
}

/// A compiled `locate` pattern.
#[derive(Debug, Clone)]
pub struct Pattern(regex::bytes::Regex);

impl Pattern {
    /// A literal, or a regex when `is_regex`, within the pattern bounds.
    ///
    /// # Errors
    /// Too long, empty, or does not compile within the size limit.
    pub fn new(query: &str, is_regex: bool, bounds: &Bounds) -> Result<Self, EvidenceError> {
        if query.is_empty() {
            return Err(EvidenceError::Pattern("empty".into()));
        }
        if query.len() > bounds.pattern {
            return Err(EvidenceError::Pattern(format!(
                "longer than {} bytes",
                bounds.pattern
            )));
        }
        let source = if is_regex {
            query.to_owned()
        } else {
            regex::escape(query)
        };
        regex::bytes::RegexBuilder::new(&source)
            .size_limit(bounds.regex_size)
            .build()
            .map(Self)
            .map_err(|e| EvidenceError::Pattern(e.to_string()))
    }
}

/// The raw, `/`-separated path of `path` under `root`, as the scanner
/// encodes it for `.pairignore`, and its text: `Ok` when every component is
/// valid Unicode, so an anchor can address it, else `Err` with a lossy form.
fn raw_rel(root: &Path, path: &Path) -> Option<(Vec<u8>, Result<String, String>)> {
    let rel = path.strip_prefix(root).ok()?;
    let mut raw = Vec::new();
    let mut text = Vec::new();
    let mut exact = true;
    for c in rel.components() {
        let Component::Normal(p) = c else {
            return None;
        };
        if !raw.is_empty() {
            raw.push(b'/');
        }
        raw.extend_from_slice(&tree::ref_bytes(p));
        exact &= p.to_str().is_some();
        text.push(p.to_string_lossy().into_owned());
    }
    let text = text.join("/");
    Some((raw, if exact { Ok(text) } else { Err(text) }))
}

/// Find `pattern` in the tree under `root`, optionally only in files whose
/// path matches `glob` (the `.pairignore` glob grammar).
///
/// The tree's own `.gitignore` files and `.git/info/exclude` are honoured
/// (not the user's global excludes, so the answer is the same for every
/// participant); `.pairignore` as the tree scanner honours it (its own
/// grammar, never reread as a gitignore). Links, the mailbox, `.git` and
/// secret-like names are never read. Anything else that is not searched is
/// named in the result, and `complete` is false.
///
/// # Errors
/// An unreadable `.pairignore`.
pub fn locate(
    root: &Path,
    pattern: &Pattern,
    glob: Option<&str>,
    bounds: &Bounds,
) -> Result<Located, EvidenceError> {
    let pats = tree::pairignore(root)
        .map_err(|e| EvidenceError::Io(tree::PAIRIGNORE.to_owned(), e.to_string()))?;
    let prunes = tree::prune_pats(&pats);
    let started = Instant::now();
    let mut out = Located::default();
    let mut bytes_read: u64 = 0;
    let filter_root = root.to_path_buf();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .parents(false)
        .git_global(false)
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            if tree::is_link(e.path()) {
                return false;
            }
            let Some((raw, _)) = raw_rel(&filter_root, e.path()) else {
                return false;
            };
            if is_reserved(e.file_name()) {
                return false;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            if is_dir {
                return !tree::glob_match_raw(&prunes, &raw);
            }
            pats.is_empty() || !tree::glob_match_raw(&pats, &raw)
        })
        .sort_by_file_path(|a, b| a.cmp(b))
        .build();
    'walk: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                out.unreadable.push(e.to_string());
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some((raw, rel)) = raw_rel(root, entry.path()) else {
            continue;
        };
        if localpilot_sandbox::is_secret_like(entry.path()) {
            continue;
        }
        if let Some(g) = glob {
            if !tree::glob_is_match_raw(g, &raw) {
                continue;
            }
        }
        let rel = match rel {
            Ok(r) => r,
            Err(lossy) => {
                out.unaddressable.push(lossy);
                continue;
            }
        };
        if started.elapsed() >= bounds.wall {
            out.truncated = Some("wall");
            break;
        }
        if out.files_read >= bounds.files {
            out.truncated = Some("files");
            break;
        }
        let remaining = bounds.total_bytes - bytes_read;
        let limit = bounds.file_bytes.min(remaining);
        let bytes = match read_file(root, &rel, limit) {
            Ok(b) => b,
            Err(EvidenceError::TooLarge(..)) if limit < bounds.file_bytes => {
                out.truncated = Some("total_bytes");
                break;
            }
            Err(EvidenceError::TooLarge(..)) => {
                out.too_large.push(rel);
                continue;
            }
            Err(EvidenceError::Changed(_) | EvidenceError::Missing(_) | EvidenceError::Link(_)) => {
                out.changed_during_read.push(rel);
                continue;
            }
            Err(e) => {
                out.unreadable.push(e.to_string());
                continue;
            }
        };
        out.files_read += 1;
        bytes_read += bytes.len() as u64;
        for (i, line) in split_lines(&bytes).iter().enumerate() {
            if !pattern.0.is_match(line) {
                continue;
            }
            if out.hits.len() >= bounds.hits {
                out.truncated = Some("hits");
                break 'walk;
            }
            out.hits.push(Hit {
                anchor: Anchor {
                    path: rel.clone(),
                    start: i + 1,
                    end: i + 1,
                    sha: hash_lines(std::slice::from_ref(line)),
                },
                snippet: snippet(line, bounds.snippet),
            });
        }
    }
    out.complete = out.truncated.is_none()
        && out.changed_during_read.is_empty()
        && out.too_large.is_empty()
        && out.unreadable.is_empty()
        && out.unaddressable.is_empty();
    Ok(out)
}

/// `line` lossily decoded, cut to at most `max` bytes at a character
/// boundary.
fn snippet(line: &[u8], max: usize) -> String {
    let s = String::from_utf8_lossy(line);
    if s.len() <= max {
        return s.into_owned();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, body) in files {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, body).unwrap();
        }
        dir
    }

    fn b() -> Bounds {
        Bounds::default()
    }

    #[test]
    fn an_anchor_hashes_lines_with_crlf_read_as_lf() {
        let d = tree_with(&[
            ("a.txt", "one\r\ntwo\r\nthree\r\n"),
            ("b.txt", "one\ntwo\nthree\n"),
        ]);
        let a = anchor(d.path(), "a.txt", 2, 3, &b()).unwrap();
        let bb = anchor(d.path(), "b.txt", 2, 3, &b()).unwrap();
        assert_eq!(a.anchor.sha, bb.anchor.sha);
        assert_eq!(a.text, "two\nthree");
        assert_eq!(a.anchor.sha, tree::hex(&Sha256::digest(b"two\nthree")));
    }

    #[test]
    fn a_whole_file_anchor_counts_every_line() {
        let d = tree_with(&[("a.txt", "a\n\nb\n"), ("b.txt", "a\n\nb"), ("e.txt", "")]);
        for f in ["a.txt", "b.txt"] {
            let w = anchor_file(d.path(), f, &b()).unwrap().anchor;
            assert_eq!((w.start, w.end), (1, 3), "{f}");
            assert_eq!(w.sha, anchor(d.path(), f, 1, 3, &b()).unwrap().anchor.sha);
        }
        assert!(matches!(
            anchor_file(d.path(), "e.txt", &b()),
            Err(EvidenceError::BadRange { .. })
        ));
    }

    #[test]
    fn a_range_outside_the_file_is_refused() {
        let d = tree_with(&[("a.txt", "one\ntwo\n")]);
        for (s, e) in [(0, 1), (2, 1), (1, 3)] {
            assert!(matches!(
                anchor(d.path(), "a.txt", s, e, &b()),
                Err(EvidenceError::BadRange { .. })
            ));
        }
    }

    #[test]
    fn verify_reports_ok_moved_ambiguous_and_stale() {
        let d = tree_with(&[("a.txt", "x\nkeep\nme\ny\n")]);
        let a = anchor(d.path(), "a.txt", 2, 3, &b()).unwrap().anchor;
        assert_eq!(verify(d.path(), &a, &b(), &mut 0), Check::Ok);
        fs::write(d.path().join("a.txt"), "new\nx\nkeep\nme\ny\n").unwrap();
        assert_eq!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Moved { start: 3, end: 4 }
        );
        fs::write(d.path().join("a.txt"), "keep\nme\nx\nkeep\nme\n").unwrap();
        assert_eq!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Ambiguous { places: 2 }
        );
        fs::write(d.path().join("a.txt"), "x\nkeep\nyou\n").unwrap();
        assert!(matches!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Stale { .. }
        ));
        fs::remove_file(d.path().join("a.txt")).unwrap();
        assert_eq!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Stale {
                why: "the file is gone".into()
            }
        );
    }

    #[test]
    fn verify_checks_the_current_bytes_not_a_commit() {
        // A dirty tree is normal: the anchor is of the file on disk.
        let d = tree_with(&[("a.txt", "committed\n")]);
        fs::write(d.path().join("a.txt"), "edited\n").unwrap();
        let a = anchor(d.path(), "a.txt", 1, 1, &b()).unwrap().anchor;
        assert_eq!(verify(d.path(), &a, &b(), &mut 0), Check::Ok);
    }

    #[test]
    fn a_made_up_hash_never_verifies() {
        let d = tree_with(&[("a.txt", "x\n")]);
        let fake = |sha: &str| Anchor {
            path: "a.txt".into(),
            start: 1,
            end: 1,
            sha: sha.into(),
        };
        assert!(matches!(
            verify(d.path(), &fake("abc"), &b(), &mut 0),
            Check::Stale { .. }
        ));
        assert!(matches!(
            verify(d.path(), &fake(&"0".repeat(64)), &b(), &mut 0),
            Check::Stale { .. }
        ));
    }

    #[test]
    fn paths_outside_the_tree_metadata_and_secrets_are_refused() {
        let d = tree_with(&[("a.txt", "x\n"), (".env", "K=V\n"), (".git/config", "x\n")]);
        let refuse = |rel: &str| anchor(d.path(), rel, 1, 1, &b()).unwrap_err();
        assert!(matches!(refuse("../a.txt"), EvidenceError::OutsideTree(_)));
        assert!(matches!(refuse(""), EvidenceError::OutsideTree(_)));
        let abs = d.path().join("a.txt").display().to_string();
        assert!(matches!(refuse(&abs), EvidenceError::OutsideTree(_)));
        assert!(matches!(refuse(".env"), EvidenceError::SecretLike(_)));
        assert!(matches!(refuse(".git/config"), EvidenceError::Metadata(_)));
        assert!(matches!(
            refuse(".pair-programming/x"),
            EvidenceError::Metadata(_)
        ));
    }

    #[test]
    fn locate_finds_literal_and_regex_hits_with_anchors_that_verify() {
        let d = tree_with(&[
            ("src/a.rs", "fn main() {}\nfn other() {}\n"),
            ("b.md", "no\n"),
        ]);
        let lit = Pattern::new("fn other(", false, &b()).unwrap();
        let found = locate(d.path(), &lit, None, &b()).unwrap();
        assert_eq!(found.hits.len(), 1);
        let hit = &found.hits[0];
        assert_eq!(
            (hit.anchor.path.as_str(), hit.anchor.start),
            ("src/a.rs", 2)
        );
        assert_eq!(verify(d.path(), &hit.anchor, &b(), &mut 0), Check::Ok);
        let re = Pattern::new(r"^fn \w+", true, &b()).unwrap();
        assert_eq!(locate(d.path(), &re, None, &b()).unwrap().hits.len(), 2);
        let only_md = locate(d.path(), &re, Some("*.md"), &b()).unwrap();
        assert!(only_md.hits.is_empty());
    }

    #[test]
    fn locate_skips_gitignored_metadata_and_secret_files() {
        let d = tree_with(&[
            (".gitignore", "ignored.txt\n"),
            ("ignored.txt", "needle\n"),
            (".env", "needle\n"),
            (".pair-programming/j.jsonl", "needle\n"),
            ("kept.txt", "needle\n"),
        ]);
        fs::create_dir(d.path().join(".git")).unwrap();
        fs::write(d.path().join(".git").join("x"), "needle\n").unwrap();
        let p = Pattern::new("needle", false, &b()).unwrap();
        let paths: Vec<String> = locate(d.path(), &p, None, &b())
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.anchor.path)
            .collect();
        assert_eq!(paths, vec!["kept.txt"]);
    }

    #[test]
    fn locate_prunes_as_the_tree_scanner_does() {
        // The scanner's grammar: `build/**`, `build/` and a literal path
        // prune a subtree; `foo/*` takes direct children only.
        let d = tree_with(&[
            (".pairignore", "build/**\nout/\ngen/x.txt\nfoo/*\n"),
            ("build/a.txt", "needle\n"),
            ("out/a.txt", "needle\n"),
            ("gen/x.txt", "needle\n"),
            ("gen/y.txt", "needle\n"),
            ("foo/direct.txt", "needle\n"),
            ("foo/bar/deep.txt", "needle\n"),
        ]);
        let p = Pattern::new("needle", false, &b()).unwrap();
        let paths: Vec<String> = locate(d.path(), &p, None, &b())
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.anchor.path)
            .collect();
        assert_eq!(paths, vec!["foo/bar/deep.txt", "gen/y.txt"]);
    }

    #[test]
    fn locate_stops_at_each_bound_and_says_which() {
        let d = tree_with(&[("a.txt", "n\nn\nn\n"), ("b.txt", "n\n"), ("c.txt", "n\n")]);
        let p = Pattern::new("n", false, &b()).unwrap();
        let hits = Bounds { hits: 2, ..b() };
        let r = locate(d.path(), &p, None, &hits).unwrap();
        assert_eq!((r.hits.len(), r.truncated), (2, Some("hits")));
        let files = Bounds { files: 1, ..b() };
        let r = locate(d.path(), &p, None, &files).unwrap();
        assert_eq!((r.files_read, r.truncated), (1, Some("files")));
        let total = Bounds {
            total_bytes: 7,
            ..b()
        };
        assert_eq!(
            locate(d.path(), &p, None, &total).unwrap().truncated,
            Some("total_bytes")
        );
        let big = Bounds {
            file_bytes: 3,
            ..b()
        };
        let r = locate(d.path(), &p, None, &big).unwrap();
        assert_eq!(r.too_large, vec!["a.txt"]);
        let wall = Bounds {
            wall: Duration::ZERO,
            ..b()
        };
        assert_eq!(
            locate(d.path(), &p, None, &wall).unwrap().truncated,
            Some("wall")
        );
    }

    #[test]
    fn a_pattern_past_its_bounds_is_refused() {
        assert!(Pattern::new("", false, &b()).is_err());
        assert!(Pattern::new(&"a".repeat(1_025), false, &b()).is_err());
        let small = Bounds {
            regex_size: 64,
            ..b()
        };
        assert!(Pattern::new(r"\w{50}\w{50}", true, &small).is_err());
        assert!(Pattern::new("(", true, &b()).is_err());
    }

    #[test]
    fn relocation_past_the_work_bound_is_unknown_not_stale() {
        let body = (0..200).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line {i}");
            s
        });
        let d = tree_with(&[("a.txt", &body)]);
        let a = anchor(d.path(), "a.txt", 10, 60, &b()).unwrap().anchor;
        fs::write(d.path().join("a.txt"), format!("new\n{body}")).unwrap();
        let tight = Bounds {
            relocation_bytes: 100,
            ..b()
        };
        assert!(matches!(
            verify(d.path(), &a, &tight, &mut 0),
            Check::Unknown { .. }
        ));
        // The budget is shared across one request's anchors.
        let mut spent = 0;
        assert_eq!(
            verify(d.path(), &a, &b(), &mut spent),
            Check::Moved { start: 11, end: 61 }
        );
        assert!(spent > 0);
        let used = Bounds {
            relocation_bytes: spent,
            ..b()
        };
        assert!(matches!(
            verify(d.path(), &a, &used, &mut spent),
            Check::Unknown { .. }
        ));
    }

    #[test]
    fn a_file_over_the_bound_is_never_read_past_it() {
        let d = tree_with(&[("a.txt", "0123456789\n")]);
        assert!(matches!(
            read_file(d.path(), "a.txt", 5),
            Err(EvidenceError::TooLarge(_, 5))
        ));
        assert_eq!(read_file(d.path(), "a.txt", 11).unwrap().len(), 11);
    }

    #[test]
    fn a_complete_search_says_so_and_a_partial_one_does_not() {
        let d = tree_with(&[("a.txt", "n\n"), ("big.txt", "nnnnnnnnnn\n")]);
        let p = Pattern::new("n", false, &b()).unwrap();
        assert!(locate(d.path(), &p, None, &b()).unwrap().complete);
        let small = Bounds {
            file_bytes: 4,
            ..b()
        };
        let r = locate(d.path(), &p, None, &small).unwrap();
        assert!(!r.complete);
        assert_eq!(r.too_large, vec!["big.txt"]);
    }

    #[test]
    fn a_file_that_cannot_be_checked_is_unknown_not_stale() {
        let d = tree_with(&[("a.txt", "0123456789\n")]);
        let a = anchor(d.path(), "a.txt", 1, 1, &b()).unwrap().anchor;
        let small = Bounds {
            file_bytes: 4,
            ..b()
        };
        assert!(matches!(
            verify(d.path(), &a, &small, &mut 0),
            Check::Unknown { .. }
        ));
        // A directory where the file was: gone as a file.
        fs::remove_file(d.path().join("a.txt")).unwrap();
        fs::create_dir(d.path().join("a.txt")).unwrap();
        assert!(matches!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Stale { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_or_linked_file_is_unknown() {
        use std::os::unix::fs::PermissionsExt;
        let d = tree_with(&[("a.txt", "x\n"), ("t.txt", "x\n")]);
        let a = anchor(d.path(), "a.txt", 1, 1, &b()).unwrap().anchor;
        fs::remove_file(d.path().join("a.txt")).unwrap();
        std::os::unix::fs::symlink(d.path().join("t.txt"), d.path().join("a.txt")).unwrap();
        assert!(matches!(
            verify(d.path(), &a, &b(), &mut 0),
            Check::Unknown { .. }
        ));
        fs::remove_file(d.path().join("a.txt")).unwrap();
        fs::write(d.path().join("a.txt"), "x\n").unwrap();
        fs::set_permissions(d.path().join("a.txt"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(d.path().join("a.txt")).is_err() {
            // Not running as root, so the file really is unreadable.
            assert!(matches!(
                verify(d.path(), &a, &b(), &mut 0),
                Check::Unknown { .. }
            ));
        }
        fs::set_permissions(d.path().join("a.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn reserved_names_are_refused_in_any_case() {
        let d = tree_with(&[
            ("a.txt", "x\n"),
            (".git/config", "x\n"),
            (".pair-programming/j", "x\n"),
        ]);
        for rel in [
            ".GIT/config",
            ".Git/config",
            ".PAIR-PROGRAMMING/j",
            "sub/.GIT/x",
        ] {
            assert!(
                matches!(checked_path(d.path(), rel), Err(EvidenceError::Metadata(_))),
                "{rel}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_aliases_of_reserved_and_secret_names_are_refused() {
        let d = tree_with(&[(".git/config", "x\n"), (".env", "K=V\n")]);
        // Trailing dots and spaces name the same entry on Windows.
        assert!(matches!(
            checked_path(d.path(), ".git./config"),
            Err(EvidenceError::Metadata(_))
        ));
        // A short (8.3) name, where the volume creates them.
        let out = std::process::Command::new("cmd")
            .args(["/C", "dir", "/x", "/a"])
            .current_dir(d.path())
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&out.stdout);
        let short = listing
            .lines()
            .filter(|l| l.trim_end().ends_with(".env"))
            .find_map(|l| {
                l.split_whitespace()
                    .rev()
                    .nth(1)
                    .filter(|s| s.contains('~'))
                    .map(str::to_owned)
            });
        if let Some(short) = short {
            assert!(
                matches!(
                    checked_path(d.path(), &short),
                    Err(EvidenceError::SecretLike(_))
                ),
                "{short}"
            );
        }
    }

    #[test]
    fn a_snippet_is_cut_at_a_character_boundary() {
        assert_eq!(snippet("ééé".as_bytes(), 3), "é");
        assert_eq!(snippet(b"abc", 10), "abc");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed_or_read() {
        let outside = tree_with(&[("secret.txt", "needle\n")]);
        let d = tree_with(&[("a.txt", "x\n")]);
        std::os::unix::fs::symlink(outside.path(), d.path().join("out")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), d.path().join("l.txt"))
            .unwrap();
        let p = Pattern::new("needle", false, &b()).unwrap();
        assert!(locate(d.path(), &p, None, &b()).unwrap().hits.is_empty());
        assert!(matches!(
            anchor(d.path(), "l.txt", 1, 1, &b()),
            Err(EvidenceError::Link(_))
        ));
        assert!(matches!(
            anchor(d.path(), "out/secret.txt", 1, 1, &b()),
            Err(EvidenceError::Link(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_is_never_followed_or_read() {
        let outside = tree_with(&[("secret.txt", "needle\n")]);
        let d = tree_with(&[("a.txt", "x\n")]);
        let ok = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(d.path().join("out"))
            .arg(outside.path())
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            return; // no junction support here
        }
        let p = Pattern::new("needle", false, &b()).unwrap();
        assert!(locate(d.path(), &p, None, &b()).unwrap().hits.is_empty());
        assert!(matches!(
            anchor(d.path(), "out/secret.txt", 1, 1, &b()),
            Err(EvidenceError::Link(_))
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_undecodable_name_is_matched_by_its_raw_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let d = tree_with(&[(".pairignore", "skip*\n")]);
        let name = std::ffi::OsStr::from_bytes(b"skip\xff.txt");
        fs::write(d.path().join(name), "needle\n").unwrap();
        let kept = std::ffi::OsStr::from_bytes(b"k\xff.txt");
        fs::write(d.path().join(kept), "needle\n").unwrap();
        let p = Pattern::new("needle", false, &b()).unwrap();
        let found = locate(d.path(), &p, None, &b()).unwrap();
        // The pruned name is matched by its raw bytes; the kept one cannot be
        // addressed by an anchor, so it is listed, not returned as a hit
        // that would never verify.
        assert!(found.hits.is_empty());
        assert_eq!(found.unaddressable, vec!["k\u{fffd}.txt"]);
        assert!(!found.complete);
    }
}
