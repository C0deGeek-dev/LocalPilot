//! A working tree without version control, owned through a content digest.
//!
//! The digest is a review boundary only if the scan records links without
//! following them, fails on anything it cannot read, and prunes exactly what
//! `.pairignore` says, so each of those is a rule here, not a convenience:
//!
//! - rows are `(path, kind, size, sha256)`: kind `f` for a file (the hash of
//!   its bytes) and `l` for a link, size -1 and the hash of its target text.
//!   A link (a symlink, a Windows junction, any reparse point) is never
//!   descended;
//! - `.git` at any depth and the mailbox at the top are not part of the tree;
//! - every listing, stat, read and link-read error ends the scan, naming the
//!   path. An unreadable file never becomes a stable placeholder that a digest
//!   could authorise;
//! - `.pairignore` lines are globs over `/`-separated paths. `*` and `?` never
//!   cross a `/`, and `**` spans segments. Only `x/**`, `x/` and wildcard-free
//!   paths prune a whole directory.
//!
//! The manifest and digest formats are the reference's, byte for byte, so the
//! two implementations agree on the same tree.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::MeshError;
use crate::layout::MAILBOX_DIR;

/// The ignore file at the root of a no-VCS tree.
pub const PAIRIGNORE: &str = ".pairignore";

/// One manifest row. Rows sort by their exact path encoding first, as the
/// reference's rows sort by path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Row {
    /// The path relative to the tree root, `/`-separated, in the reference's
    /// encoding ([`ref_bytes`]). This is what the manifest and digest hold.
    pub raw: Vec<u8>,
    /// The same path for display and for glob matching, with any
    /// undecodable part shown as U+FFFD.
    pub path: String,
    /// `f` for a file, `l` for a link.
    pub kind: char,
    /// The size in bytes; -1 for a link.
    pub size: i64,
    /// sha256 of the file's bytes, or of a link's target text.
    pub sha256: String,
}

/// A scan that could not read part of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanError {
    /// What failed: `list`, `stat`, `read`, or `read the link at`.
    pub what: &'static str,
    /// The path that failed, relative to the tree root (`.` for the root).
    pub path: String,
    pub detail: String,
}

impl ScanError {
    fn into_mesh(self, root: &Path) -> MeshError {
        MeshError::Refused(format!(
            "cannot {} '{}' under {}: {}\n  the content digest covers every file, so a path it cannot read leaves the boundary undefined;\n  fix the permission, or exclude the path in {PAIRIGNORE} where both roles can see it",
            self.what,
            self.path,
            root.display(),
            self.detail
        ))
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

// --- globs -----------------------------------------------------------------

#[derive(Debug, Clone)]
enum Tok {
    /// `**/`: zero or more whole segments.
    AnySegments,
    /// `**`: anything, `/` included.
    AnyAll,
    /// `*`: anything within one segment.
    AnyInSegment,
    /// `?`: one character, not `/`.
    One,
    /// `[...]`: a class (ranges allowed), possibly negated.
    Class {
        negated: bool,
        items: Vec<(u32, u32)>,
    },
    Lit(u32),
}

const SLASH: u32 = '/' as u32;

fn compile(pat: &str) -> Vec<Tok> {
    let c: Vec<char> = pat.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '*' if c.get(i + 1) == Some(&'*') && c.get(i + 2) == Some(&'/') => {
                out.push(Tok::AnySegments);
                i += 3;
            }
            '*' if c.get(i + 1) == Some(&'*') => {
                out.push(Tok::AnyAll);
                i += 2;
            }
            '*' => {
                out.push(Tok::AnyInSegment);
                i += 1;
            }
            '?' => {
                out.push(Tok::One);
                i += 1;
            }
            '[' => {
                let mut j = i + 1;
                if j < c.len() && (c[j] == '!' || c[j] == '^') {
                    j += 1;
                }
                if j < c.len() && c[j] == ']' {
                    j += 1;
                }
                while j < c.len() && c[j] != ']' {
                    j += 1;
                }
                if j >= c.len() {
                    out.push(Tok::Lit('[' as u32));
                    i += 1;
                    continue;
                }
                let inner: Vec<char> = c[i + 1..j].to_vec();
                let (negated, body) = match inner.first() {
                    Some('!' | '^') => (true, &inner[1..]),
                    _ => (false, &inner[..]),
                };
                let mut items = Vec::new();
                let mut k = 0;
                while k < body.len() {
                    if k + 2 < body.len() && body[k + 1] == '-' {
                        items.push((body[k] as u32, body[k + 2] as u32));
                        k += 3;
                    } else {
                        items.push((body[k] as u32, body[k] as u32));
                        k += 1;
                    }
                }
                out.push(Tok::Class { negated, items });
                i = j + 1;
            }
            ch => {
                out.push(Tok::Lit(ch as u32));
                i += 1;
            }
        }
    }
    out
}

/// Matching runs over code points, not `char`s: a path's undecodable parts
/// are surrogate code points (as the reference sees them), which no pattern
/// character, U+FFFD included, is equal to.
fn matches_at(toks: &[Tok], s: &[u32]) -> bool {
    let Some((t, rest)) = toks.split_first() else {
        return s.is_empty();
    };
    match t {
        Tok::Lit(ch) => s.first() == Some(ch) && matches_at(rest, &s[1..]),
        Tok::One => s.first().is_some_and(|c| *c != SLASH) && matches_at(rest, &s[1..]),
        Tok::Class { negated, items } => {
            s.first().is_some_and(|c| {
                let hit = items.iter().any(|(lo, hi)| lo <= c && c <= hi);
                hit != *negated
            }) && matches_at(rest, &s[1..])
        }
        Tok::AnyInSegment => {
            let limit = s.iter().position(|c| *c == SLASH).unwrap_or(s.len());
            (0..=limit).any(|n| matches_at(rest, &s[n..]))
        }
        Tok::AnyAll => (0..=s.len()).any(|n| matches_at(rest, &s[n..])),
        Tok::AnySegments => {
            // Zero or more `segment/` groups, each segment non-empty.
            if matches_at(rest, s) {
                return true;
            }
            let mut pos = 0;
            loop {
                let Some(off) = s[pos..].iter().position(|c| *c == SLASH) else {
                    return false;
                };
                if off == 0 {
                    return false;
                }
                pos += off + 1;
                if matches_at(rest, &s[pos..]) {
                    return true;
                }
            }
        }
    }
}

/// Whether `rel` (POSIX form) matches `pat`. Case-sensitive everywhere.
#[must_use]
pub fn glob_is_match(pat: &str, rel: &str) -> bool {
    let cps: Vec<u32> = rel.chars().map(|c| c as u32).collect();
    matches_at(&compile(pat), &cps)
}

/// The code points of a path in the reference encoding ([`ref_bytes`]):
/// UTF-8 in which a surrogate code point may also appear in its three-byte
/// form. Each surrogate stays a surrogate, as the reference's `str` holds it.
fn code_points(raw: &[u8]) -> Vec<u32> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        let (len, init) = match b {
            0x00..=0x7F => (1, u32::from(b)),
            0xC0..=0xDF => (2, u32::from(b & 0x1F)),
            0xE0..=0xEF => (3, u32::from(b & 0x0F)),
            _ => (4, u32::from(b & 0x07)),
        };
        let mut cp = init;
        for k in 1..len {
            cp = (cp << 6) | u32::from(raw.get(i + k).copied().unwrap_or(0x80) & 0x3F);
        }
        out.push(cp);
        i += len;
    }
    out
}

/// Whether a path, given in the reference encoding, matches `pat`.
#[must_use]
pub fn glob_is_match_raw(pat: &str, raw: &[u8]) -> bool {
    matches_at(&compile(pat), &code_points(raw))
}

/// Whether a path, given in the reference encoding, matches any of `pats`.
#[must_use]
pub fn glob_match_raw(pats: &[String], raw: &[u8]) -> bool {
    let cps = code_points(raw);
    pats.iter().any(|p| matches_at(&compile(p), &cps))
}

/// Whether `rel` matches any of `pats`.
#[must_use]
pub fn glob_match(pats: &[String], rel: &str) -> bool {
    pats.iter().any(|p| glob_is_match(p, rel))
}

// --- .pairignore ------------------------------------------------------------

/// The globs `.pairignore` declares; none when it is absent.
///
/// # Errors
/// An ignore file that exists but cannot be read.
pub fn pairignore(root: &Path) -> Result<Vec<String>, MeshError> {
    let p = root.join(PAIRIGNORE);
    match fs::read(&p) {
        Ok(bytes) => Ok(String::from_utf8_lossy(&bytes)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(str::to_owned)
            .collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(MeshError::io(&p, e)),
    }
}

/// The directory forms of the ignore globs that cover a whole subtree.
#[must_use]
pub fn prune_pats(pats: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in pats {
        let d = if let Some(stem) = g.strip_suffix("/**") {
            stem.trim_end_matches('/').to_owned()
        } else if g.ends_with('/') {
            g.trim_end_matches('/').to_owned()
        } else if !g.contains(['*', '?', '[']) {
            g.clone()
        } else {
            continue;
        };
        if !d.is_empty() && !out.contains(&d) {
            out.push(d);
        }
    }
    out.sort();
    out
}

// --- the scan ---------------------------------------------------------------

/// A symlink, junction or other reparse point, or a path that cannot be
/// examined (fail closed).
pub(crate) fn is_link(p: &Path) -> bool {
    match fs::symlink_metadata(p) {
        Ok(m) => {
            if m.file_type().is_symlink() {
                return true;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
                if m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    return true;
                }
            }
            false
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

/// A link's target as text, in the form the reference records. Windows
/// stores an absolute target in NT form (`\??\C:\x`), which the reference
/// reports as `\\?\C:\x`, while `read_link` strips that prefix from some kinds
/// of link. The verbatim form is restored for any absolute target, so both
/// builds hash the same text for the same link.
fn link_text(target: &Path) -> Vec<u8> {
    let bytes = ref_bytes(target.as_os_str());
    if cfg!(windows) {
        let s = target.to_string_lossy();
        if s.starts_with(r"\\?\") {
            return bytes;
        }
        if s.starts_with(r"\\") {
            let mut out = br"\\?\UNC\".to_vec();
            out.extend_from_slice(&bytes[2..]);
            return out;
        }
        let b = s.as_bytes();
        if b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/')
        {
            let mut out = br"\\?\".to_vec();
            out.extend_from_slice(&bytes);
            return out;
        }
    }
    bytes
}

/// A name or path as the reference encodes it for hashing and sorting:
/// UTF-8, with each byte that is not valid UTF-8 (POSIX) written as the
/// surrogate escape U+DC00+byte, and an unpaired surrogate (Windows) written
/// as its three-byte form. Two distinct names therefore never share an
/// encoding, and a rename between two undecodable names moves the digest.
#[must_use]
pub fn ref_bytes(os: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(windows)]
    {
        // WTF-8: UTF-8 with any unpaired surrogate as its three-byte form,
        // which is exactly the reference's `surrogatepass` encoding.
        os.as_encoded_bytes().to_vec()
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut rest = os.as_bytes();
        let mut out = Vec::with_capacity(rest.len());
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.extend_from_slice(s.as_bytes());
                    return out;
                }
                Err(e) => {
                    let (valid, tail) = rest.split_at(e.valid_up_to());
                    out.extend_from_slice(valid);
                    let bad = e.error_len().unwrap_or(tail.len());
                    for b in &tail[..bad] {
                        let cp = 0xDC00_u32 + u32::from(*b);
                        // Three-byte form of a surrogate code point: ED, then
                        // two continuation bytes.
                        out.push(0xED);
                        out.push(0x80 | u8::try_from((cp >> 6) & 0x3F).unwrap_or(0));
                        out.push(0x80 | u8::try_from(cp & 0x3F).unwrap_or(0));
                    }
                    rest = &tail[bad..];
                }
            }
        }
    }
}

fn name_of(entry: &fs::DirEntry) -> Vec<u8> {
    ref_bytes(&entry.file_name())
}

/// The manifest rows of `root`, sorted.
///
/// # Errors
/// [`ScanError`] for the first path that could not be listed, examined or read.
pub fn scan_rows(root: &Path, pats: &[String]) -> Result<Vec<Row>, ScanError> {
    let prunes = prune_pats(pats);
    let mut rows = Vec::new();
    walk(root, "", &[], pats, &prunes, &mut rows)?;
    rows.sort();
    Ok(rows)
}

fn fail(what: &'static str, rel: &str, e: &io::Error) -> ScanError {
    ScanError {
        what,
        path: if rel.is_empty() {
            ".".into()
        } else {
            rel.into()
        },
        detail: e.to_string(),
    }
}

fn walk(
    dir: &Path,
    base: &str,
    base_raw: &[u8],
    pats: &[String],
    prunes: &[String],
    rows: &mut Vec<Row>,
) -> Result<(), ScanError> {
    let rd = fs::read_dir(dir).map_err(|e| fail("list", base, &e))?;
    let mut entries: Vec<fs::DirEntry> = Vec::new();
    for e in rd {
        entries.push(e.map_err(|e| fail("list", base, &e))?);
    }
    entries.sort_by_key(name_of);
    for e in entries {
        let name_raw = name_of(&e);
        let name = e.file_name().to_string_lossy().into_owned();
        let rel = if base.is_empty() {
            name.clone()
        } else {
            format!("{base}/{name}")
        };
        let mut raw = base_raw.to_vec();
        if !raw.is_empty() {
            raw.push(b'/');
        }
        raw.extend_from_slice(&name_raw);
        if (base.is_empty() && name_raw == MAILBOX_DIR.as_bytes()) || name_raw == b".git" {
            continue;
        }
        let path = e.path();
        if is_link(&path) {
            let target =
                fs::read_link(&path).map_err(|err| fail("read the link at", &rel, &err))?;
            rows.push(Row {
                raw,
                path: rel,
                kind: 'l',
                size: -1,
                sha256: hex(&Sha256::digest(link_text(&target))),
            });
            continue;
        }
        let meta = fs::symlink_metadata(&path).map_err(|err| fail("stat", &rel, &err))?;
        if meta.is_dir() {
            if !glob_match_raw(prunes, &raw) {
                walk(&path, &rel, &raw, pats, prunes, rows)?;
            }
            continue;
        }
        if !pats.is_empty() && glob_match_raw(pats, &raw) {
            continue;
        }
        let bytes = fs::read(&path).map_err(|err| fail("read", &rel, &err))?;
        rows.push(Row {
            raw,
            path: rel,
            kind: 'f',
            size: i64::try_from(meta.len()).unwrap_or(i64::MAX),
            sha256: hex(&Sha256::digest(&bytes)),
        });
    }
    Ok(())
}

/// [`scan_rows`] with the tree's own `.pairignore`, as a mailbox error.
///
/// # Errors
/// An unreadable ignore file, or any [`ScanError`].
pub fn scan(root: &Path) -> Result<Vec<Row>, MeshError> {
    let pats = pairignore(root)?;
    scan_rows(root, &pats).map_err(|e| e.into_mesh(root))
}

// --- manifests and the digest -------------------------------------------------

/// Rows as bytes: four NUL-terminated fields per row, nothing else. NUL is
/// the one byte a POSIX file name cannot hold, so no name can split a row.
#[must_use]
pub fn manifest_blob(rows: &[Row]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in rows {
        out.extend_from_slice(&r.raw);
        out.push(0);
        for field in [r.kind.to_string(), r.size.to_string(), r.sha256.clone()] {
            out.extend_from_slice(field.as_bytes());
            out.push(0);
        }
    }
    out
}

/// The tree's content digest: `T:` and the first 12 hex digits of the
/// sha256 of its manifest blob.
#[must_use]
pub fn tree_digest(rows: &[Row]) -> String {
    let h = hex(&Sha256::digest(manifest_blob(rows)));
    format!("T:{}", &h[..12])
}

/// Write a manifest atomically.
///
/// # Errors
/// The store's I/O error.
pub fn write_manifest(path: &Path, rows: &[Row]) -> Result<(), MeshError> {
    localpilot_store::atomic_write(path, &manifest_blob(rows))?;
    Ok(())
}

/// Where a unit's base manifest lives: `manifests/<key>-<unit>.mf`, with key
/// `anchor` or a companion's `c<n>`.
#[must_use]
pub fn manifest_path(session_dir: &Path, key: &str, unit: &str) -> PathBuf {
    session_dir
        .join("manifests")
        .join(format!("{key}-{unit}.mf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_star_never_crosses_a_segment_and_a_double_star_does() {
        assert!(glob_is_match("records/*.md", "records/a.md"));
        assert!(!glob_is_match("records/*.md", "records/deep/c.md"));
        assert!(glob_is_match("records/**", "records/deep/c.md"));
        assert!(glob_is_match("**/x.txt", "x.txt"));
        assert!(glob_is_match("**/x.txt", "a/b/x.txt"));
        assert!(
            !glob_is_match("**/x.txt", "/x.txt"),
            "a segment is never empty"
        );
        assert!(glob_is_match("a?c", "abc") && !glob_is_match("a?c", "a/c"));
        assert!(glob_is_match("[!x]y", "ay") && !glob_is_match("[!x]y", "xy"));
        assert!(glob_is_match("[a-c]", "b") && !glob_is_match("[a-c]", "d"));
        assert!(glob_is_match("[]]", "]"));
        assert!(
            glob_is_match("a[b", "a[b"),
            "an unterminated class is literal"
        );
        assert!(!glob_is_match("Records/*", "records/a"), "case-sensitive");
    }

    #[test]
    fn a_replacement_character_never_matches_an_undecodable_byte() {
        // An undecodable byte 0xFF is the surrogate U+DCFF in the reference's
        // encoding; a pattern holding U+FFFD must not match it.
        let raw = [b'a', 0xED, 0xB3, 0xBF, b'.', b't', b'x', b't'];
        assert!(!glob_is_match_raw("a\u{FFFD}.txt", &raw));
        assert!(
            glob_is_match_raw("a?.txt", &raw),
            "a wildcard still matches it"
        );
        assert!(glob_is_match_raw("a*", &raw));
        assert!(glob_is_match_raw("caf\u{e9}/x", "caf\u{e9}/x".as_bytes()));
    }

    #[test]
    fn only_whole_subtree_patterns_prune() {
        let pats: Vec<String> = ["build/**", "out/", "lit/dir", "foo/*", "*.log"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        assert_eq!(prune_pats(&pats), ["build", "lit/dir", "out"]);
    }

    #[test]
    fn the_manifest_frames_a_name_holding_a_newline() {
        let rows = vec![Row {
            raw: b"a\nb".to_vec(),
            path: "a\nb".into(),
            kind: 'f',
            size: 1,
            sha256: "00".into(),
        }];
        assert_eq!(manifest_blob(&rows), b"a\nb\0f\x001\x0000\0");
    }

    #[test]
    fn a_scan_skips_git_and_the_top_level_mailbox_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let w = |rel: &str| {
            let p = dir.path().join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, "x\n").unwrap();
        };
        for rel in [
            "kept.txt",
            ".pair-programming/active.json",
            "sub/.pair-programming/kept.txt",
            ".git/HEAD",
            "sub/.git/config",
            "build/a",
            "foo/a",
            "foo/bar/x",
        ] {
            w(rel);
        }
        fs::write(dir.path().join(PAIRIGNORE), "build/**\nfoo/*\n").unwrap();
        let paths: Vec<String> = scan(dir.path())
            .unwrap()
            .into_iter()
            .map(|r| r.path)
            .collect();
        assert_eq!(
            paths,
            [
                ".pairignore",
                "foo/bar/x",
                "kept.txt",
                "sub/.pair-programming/kept.txt"
            ]
        );
    }
}
