//! Review snapshots (spec RS-1..RS-6): a unit closes only on the bytes its
//! reviewers saw.
//!
//! In a session that requires `review-snapshot`, the owner's
//! `REVIEW_REQUEST` records every path changed in the unit, in the anchor and
//! inside each companion's write scope, with the SHA-256 of its working-tree
//! bytes. The snapshot is computed before the request is appended and is
//! published as the session's `review` only after the append lands.
//! `complete` and `next-unit` recompute it and refuse on any difference, and
//! a reviewer checks it against the tree before it agrees.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::post::PostArgs;
use super::unit::git;
use super::{participants, refused, seq_of, sid, str_of, Mesh, Obj, Out};
use crate::error::MeshError;
use crate::jsonl;
use crate::layout::MAILBOX_DIR;
use crate::timefmt::utc_now;
use crate::tree;

/// The session feature this module implements (spec V-8).
pub const FEATURE: &str = "review-snapshot";

/// `{tree: {path: value}}`: the anchor as `anchor`, each companion by its
/// manifest key.
pub type Trees = BTreeMap<String, BTreeMap<String, String>>;

/// The first line of the note that withdraws a request.
const WITHDRAWN: &str = "REVIEW_WITHDRAWN ";

/// Whether session `s` requires review snapshots.
pub(crate) fn snapshot_on(s: &Obj) -> bool {
    s.get("requires")
        .and_then(Value::as_array)
        .is_some_and(|l| l.iter().any(|f| f.as_str() == Some(FEATURE)))
}

/// The session's current review, if one is published.
pub(crate) fn current_review(s: &Obj) -> Option<&Obj> {
    s.get("review").and_then(Value::as_object)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

/// Resolve `p` through every symbolic link on its way, and lexically past
/// the first component that does not exist. A link whose target is missing
/// is still followed, so a dangling link out of the tree resolves outside it.
fn resolve_lenient(p: &Path) -> std::io::Result<PathBuf> {
    resolve_within(p, 0)
}

fn resolve_within(p: &Path, depth: usize) -> std::io::Result<PathBuf> {
    const MAX_LINKS: usize = 40;
    if depth > MAX_LINKS {
        return Err(std::io::Error::other("too many levels of symbolic links"));
    }
    let mut rest = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        let found = match std::fs::canonicalize(&cur) {
            Ok(base) => Some(base),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // An existing link to a missing target: follow it anyway.
                let link =
                    std::fs::symlink_metadata(&cur).is_ok_and(|m| m.file_type().is_symlink());
                if link {
                    let target = std::fs::read_link(&cur)?;
                    let target = if target.is_absolute() {
                        target
                    } else {
                        cur.parent()
                            .map_or_else(PathBuf::new, Path::to_path_buf)
                            .join(target)
                    };
                    Some(resolve_within(&target, depth + 1)?)
                } else {
                    let Some(name) = cur.file_name().map(std::ffi::OsStr::to_os_string) else {
                        return Err(e);
                    };
                    rest.push(name);
                    if !cur.pop() {
                        return Err(e);
                    }
                    None
                }
            }
            Err(e) => return Err(e),
        };
        if let Some(mut base) = found {
            for c in rest.iter().rev() {
                base.push(c);
            }
            return Ok(base);
        }
    }
}

/// One path's snapshot value (RS-1): a regular file's byte digest, a link's
/// target digest (never followed), `gone`, `unreadable` or `unsupported`.
pub(crate) fn entry_value(root: &Path, rel: &str) -> String {
    let p = root.join(rel);
    // A path whose parent leaves the tree is never read.
    let inside = match (std::fs::canonicalize(root), p.parent().map(resolve_lenient)) {
        (Ok(r), Some(Ok(parent))) => parent == r || parent.starts_with(&r),
        (Ok(_), None) => false,
        _ => return "unreadable".into(),
    };
    if !inside
        || Path::new(rel)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return "unsupported".into();
    }
    let meta = match std::fs::symlink_metadata(&p) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return "gone".into(),
        Err(_) => return "unreadable".into(),
    };
    if meta.file_type().is_symlink() {
        return match std::fs::read_link(&p) {
            Ok(t) => format!(
                "link:{}",
                hex(&Sha256::digest(tree::ref_bytes(
                    reference_link_text(t).as_os_str()
                )))
            ),
            Err(_) => "unreadable".into(),
        };
    }
    if !meta.is_file() {
        return "unsupported".into();
    }
    std::fs::read(&p).map_or_else(|_| "unreadable".into(), |b| hex(&Sha256::digest(b)))
}

/// A link's target as the reference reads it. On Windows Python's
/// `os.readlink` returns an absolute target in its verbatim form
/// (`\\?\C:\...`, `\\?\UNC\server\share`), where Rust's `read_link` drops
/// that prefix; both name the same target, so the verbatim form is restored.
/// A relative target, and every target elsewhere, is unchanged.
fn reference_link_text(target: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        use std::path::Prefix;
        let mut parts = target.components();
        let Some(Component::Prefix(prefix)) = parts.next() else {
            return target;
        };
        let mut v = std::ffi::OsString::from(r"\\?\");
        match prefix.kind() {
            Prefix::Disk(_) => v.push(prefix.as_os_str()),
            Prefix::UNC(server, share) => {
                v.push("UNC\\");
                v.push(server);
                v.push("\\");
                v.push(share);
            }
            // Already verbatim, or a device path: as read.
            _ => return target,
        }
        let mut out = PathBuf::from(v);
        out.push(parts.as_path());
        out
    }
    #[cfg(not(windows))]
    target
}

/// A JSON string as Python's `json.dumps` writes it by default: ASCII only,
/// every other character as a lowercase `\uXXXX` escape (a surrogate pair
/// beyond the BMP).
fn ascii_json_string(out: &mut String, s: &str) {
    use std::fmt::Write as _;
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for u in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{u:04x}");
                }
            }
        }
    }
    out.push('"');
}

/// The canonical JSON of a snapshot: sorted keys, no spaces, ASCII escapes,
/// byte-for-byte what every other implementation hashes.
fn canonical(trees: &Trees) -> String {
    let mut out = String::from("{");
    for (i, (key, paths)) in trees.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        ascii_json_string(&mut out, key);
        out.push_str(":{");
        for (j, (path, value)) in paths.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            ascii_json_string(&mut out, path);
            out.push(':');
            ascii_json_string(&mut out, value);
        }
        out.push('}');
    }
    out.push('}');
    out
}

/// A snapshot's digest: `S:` and 12 hex digits (RS-1).
pub(crate) fn digest(trees: &Trees) -> String {
    format!(
        "S:{}",
        &hex(&Sha256::digest(canonical(trees).as_bytes()))[..12]
    )
}

/// A published snapshot read back from the session record.
pub(crate) fn trees_from(v: Option<&Value>) -> Trees {
    trees_of(v)
}

fn trees_of(v: Option<&Value>) -> Trees {
    v.and_then(Value::as_object)
        .map(|t| {
            t.iter()
                .map(|(k, paths)| {
                    let paths = paths
                        .as_object()
                        .map(|p| {
                            p.iter()
                                .map(|(path, val)| {
                                    (path.clone(), val.as_str().unwrap_or_default().to_owned())
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (k.clone(), paths)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn companions(s: &Obj) -> Vec<Obj> {
    s.get("companions")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_object().cloned()).collect())
        .unwrap_or_default()
}

/// A companion's frozen manifest key, else its position.
fn ckey(c: &Obj, i: usize) -> String {
    str_of(c, "mkey").map_or_else(|| format!("c{i}"), str::to_owned)
}

/// How a path is named in a refusal: bare in the anchor, `<companion>:<path>`
/// elsewhere.
fn label(s: &Obj, key: &str, path: &str) -> String {
    if key == "anchor" {
        return path.to_owned();
    }
    let name = companions(s)
        .iter()
        .enumerate()
        .find(|(i, c)| ckey(c, *i) == key)
        .and_then(|(_, c)| str_of(c, "name").map(str::to_owned))
        .unwrap_or_else(|| key.to_owned());
    format!("{name}:{path}")
}

/// Paths whose value can never be reviewed as bytes.
pub(crate) fn problems(s: &Obj, trees: &Trees) -> Vec<String> {
    trees
        .iter()
        .flat_map(|(k, t)| {
            t.iter()
                .filter(|(_, v)| matches!(v.as_str(), "unreadable" | "unsupported"))
                .map(move |(p, _)| label(s, k, p))
        })
        .collect()
}

/// Every path whose value differs between two snapshots.
pub(crate) fn changes(s: &Obj, old: &Trees, new: &Trees) -> Vec<String> {
    let empty = BTreeMap::new();
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut out = Vec::new();
    for k in keys {
        let (a, b) = (old.get(k).unwrap_or(&empty), new.get(k).unwrap_or(&empty));
        let mut paths: Vec<&String> = a.keys().chain(b.keys()).collect();
        paths.sort();
        paths.dedup();
        out.extend(
            paths
                .into_iter()
                .filter(|p| a.get(*p) != b.get(*p))
                .map(|p| label(s, k, p)),
        );
    }
    out
}

/// A request's fingerprint lines and path count, or why none can be made.
pub(crate) type Listing = Result<(String, usize), String>;

/// The files under review, or why they cannot be reviewed.
pub type Inventory = Result<Vec<String>, Vec<String>>;

/// One manifest row: kind, size and SHA-256.
type Row = (String, String, String);

/// A tree's manifest as `{path: (kind, size, sha256)}`, `None` when there is
/// none. A damaged manifest is refused: a dropped row would hide a change.
fn read_manifest(path: &Path) -> Result<Option<BTreeMap<String, Row>>, MeshError> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(MeshError::io(path, e)),
    };
    let fields: Vec<&[u8]> = raw.split(|b| *b == 0).collect();
    let bad = || MeshError::Corrupt(format!("the unit manifest {} is damaged", path.display()));
    let Some((last, rows)) = fields.split_last() else {
        return Ok(Some(BTreeMap::new()));
    };
    if !last.is_empty() || rows.len() % 4 != 0 {
        return Err(bad());
    }
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    Ok(Some(
        rows.chunks(4)
            .map(|r| (text(r[0]), (text(r[1]), text(r[2]), text(r[3]))))
            .collect(),
    ))
}

/// Why a Git read failed, as a refusal: a failed read is never "nothing
/// changed".
fn git_failed(what: &str) -> MeshError {
    refused(format!(
        "git {what} failed; a review snapshot cannot be taken"
    ))
}

/// Every path changed in a Git tree since `base`, both sides of a rename
/// included, relative to `root`.
fn git_changed(root: &Path, base: &str) -> Result<Vec<String>, MeshError> {
    let (rc, prefix) = git(root, &["rev-parse", "--show-prefix"])?;
    if rc != Some(0) {
        return Err(git_failed("rev-parse"));
    }
    let prefix = prefix.trim().to_owned();
    let range = format!("{base}..HEAD");
    let committed = if base == "UNBORN" {
        git(
            root,
            &["ls-tree", "-r", "--full-name", "--name-only", "-z", "HEAD"],
        )?
    } else {
        git(root, &["diff", "--name-only", "-z", "--no-renames", &range])?
    };
    if committed.0 != Some(0) {
        return Err(git_failed(if base == "UNBORN" {
            "ls-tree"
        } else {
            "diff"
        }));
    }
    let mut paths: Vec<String> = committed
        .1
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect();
    let (rc, out) = git(
        root,
        &["status", "-z", "--porcelain=v1", "--untracked-files=all"],
    )?;
    if rc != Some(0) {
        return Err(git_failed("status"));
    }
    let fields: Vec<&str> = out.split('\0').filter(|f| !f.is_empty()).collect();
    let mut i = 0;
    while i < fields.len() {
        let entry = fields[i];
        let code = &entry[..entry.len().min(2)];
        paths.push(entry.get(3..).unwrap_or_default().to_owned());
        if code.contains('R') || code.contains('C') {
            if let Some(source) = fields.get(i + 1) {
                paths.push((*source).to_owned());
                i += 1;
            }
        }
        i += 1;
    }
    Ok(paths
        .into_iter()
        .filter_map(|p| p.strip_prefix(prefix.as_str()).map(str::to_owned))
        .filter(|p| !p.is_empty())
        .collect())
}

/// Every path changed in a tree with no version control: each row that
/// differs from the unit's manifest, or the whole tree without one.
fn manifest_changed(root: &Path, manifest: &Path) -> Result<Vec<String>, MeshError> {
    let live: BTreeMap<String, Row> = tree::scan(root)?
        .into_iter()
        .map(|r| (r.path, (r.kind.to_string(), r.size.to_string(), r.sha256)))
        .collect();
    let Some(was) = read_manifest(manifest)? else {
        return Ok(live.into_keys().collect());
    };
    let mut all: Vec<&String> = live.keys().chain(was.keys()).collect();
    all.sort();
    all.dedup();
    Ok(all
        .into_iter()
        .filter(|p| live.get(*p) != was.get(*p))
        .cloned()
        .collect())
}

/// Where a published review stands for a reviewer answering `msg_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    /// The request is the session's current review, with its snapshot.
    Current(Trees),
    /// A newer request or a withdrawal replaced it.
    Superseded,
    /// Its snapshot is not published (yet, or ever).
    Pending,
}

impl Mesh {
    /// The current unit's snapshot (RS-1).
    ///
    /// # Errors
    /// A refusal when a Git read or a tree scan fails.
    pub(crate) fn review_snapshot(&self, s: &Obj) -> Result<Trees, MeshError> {
        let root = self.repo();
        let sd = self.mb.session_dir(sid(s));
        let unit = str_of(s, "unit_id").unwrap_or_default();
        let anchor = if self.no_vcs(s)? {
            manifest_changed(root, &tree::manifest_path(&sd, "anchor", unit))?
        } else {
            git_changed(root, str_of(s, "base_head").unwrap_or("UNBORN"))?
        };
        let mut trees = Trees::new();
        trees.insert(
            "anchor".into(),
            anchor
                .into_iter()
                .filter(|p| p != MAILBOX_DIR && !p.starts_with(&format!("{MAILBOX_DIR}/")))
                .map(|p| {
                    let v = entry_value(root, &p);
                    (p, v)
                })
                .collect(),
        );
        for (i, c) in companions(s).iter().enumerate() {
            let croot = PathBuf::from(str_of(c, "root").unwrap_or_default());
            let key = ckey(c, i);
            let changed = if str_of(c, "vcs") == Some("none") {
                manifest_changed(&croot, &tree::manifest_path(&sd, &key, unit))?
            } else {
                git_changed(&croot, str_of(c, "head").unwrap_or("UNBORN"))?
            };
            let scope: Vec<String> = c
                .get("write")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            trees.insert(
                key,
                changed
                    .into_iter()
                    .filter(|p| tree::glob_match(&scope, p))
                    .map(|p| {
                        let v = entry_value(&croot, &p);
                        (p, v)
                    })
                    .collect(),
            );
        }
        Ok(trees)
    }

    /// Refuse a close whose agreement does not answer the current request, or
    /// whose reviewed bytes changed since (RS-4).
    ///
    /// # Errors
    /// The RS-4 refusal that applies.
    pub(crate) fn snapshot_close_check(&self, s: &Obj, agrees: &[Obj]) -> Result<(), MeshError> {
        let Some(rv) = current_review(s).filter(|rv| rv.get("unit_id") == s.get("unit_id")) else {
            return Err(refused(
                "REVIEW_SNAPSHOT_MISSING: there is no current review request in this unit; post a REVIEW_REQUEST and get a verdict on it",
            ));
        };
        let request = str_of(rv, "request").unwrap_or_default();
        let author = str_of(rv, "role").unwrap_or_default();
        let rseq = rv.get("seq").and_then(Value::as_i64).unwrap_or(0);
        // The journal, not the published state, is what the owner said last.
        let newer = self.later_mutations(s, author, rseq)?;
        if !newer.is_empty() {
            return Err(refused(format!(
                "REVIEW_SNAPSHOT_UNSETTLED request={request} newer={}: a later review request or withdrawal is not the published review; wait for it to settle, or post a new REVIEW_REQUEST",
                newer.join(",")
            )));
        }
        let stale: Vec<String> = agrees
            .iter()
            .filter(|m| str_of(m, "reply_to") != Some(request))
            .map(mid_of)
            .collect();
        if !stale.is_empty() {
            return Err(refused(format!(
                "REVIEW_SNAPSHOT_STALE request={request}: the AGREE ({}) does not answer the current review request; an AGREE must be posted with --reply-to the current request",
                stale.join(", ")
            )));
        }
        let now = self.review_snapshot(s)?;
        let bad = problems(s, &now);
        if !bad.is_empty() {
            return Err(refused(format!(
                "REVIEW_SNAPSHOT_UNSUPPORTED paths={}: these cannot be reviewed as bytes",
                bad.join(",")
            )));
        }
        let changed = changes(s, &trees_of(rv.get("trees")), &now);
        if !changed.is_empty() {
            return Err(refused(format!(
                "REVIEW_SNAPSHOT_CHANGED request={request} paths={}: the reviewed set changed after the request; post a new REVIEW_REQUEST (it supersedes the old one) and get a fresh verdict",
                changed.join(",")
            )));
        }
        Ok(())
    }

    /// `review-withdraw`: the owner clears its current request (RS-3).
    ///
    /// # Errors
    /// A session without snapshots, a non-owner, no current request, or a
    /// failed post.
    pub fn review_withdraw(&self, role: &str, reason: Option<&str>) -> Result<Out, MeshError> {
        let s = self.require(role, true)?;
        if !snapshot_on(&s) {
            return Err(refused(
                "this session does not use review snapshots; there is nothing to withdraw",
            ));
        }
        if str_of(&s, "owner") != Some(role) {
            return Err(refused(
                "only the current owner may withdraw its review request",
            ));
        }
        let Some(rv) = current_review(&s) else {
            return Err(refused("no review request is current in this unit"));
        };
        let request = str_of(rv, "request").unwrap_or_default().to_owned();
        let why = reason.map(str::trim).filter(|r| !r.is_empty());
        let body = why.map_or_else(
            || format!("{WITHDRAWN}request={request}"),
            |w| format!("{WITHDRAWN}request={request}\n{w}"),
        );
        // Appended only while `role` still owns this unit of this session.
        let expect = super::post::Expect {
            session_id: sid(&s).to_owned(),
            unit_id: str_of(&s, "unit_id").map(str::to_owned),
            reviewer: false,
            owner: true,
        };
        let m = self.post_message(
            role,
            &PostArgs {
                kind: "NOTE".into(),
                body,
                ..PostArgs::default()
            },
            Some(sid(&s)),
            true,
            Some(&expect),
        )?;
        {
            let _g = self.state_lock_after_append()?;
            // The note is durable; the state update applies only to the
            // session, unit and ownership it was written under.
            let fresh = self.active()?.filter(|f| {
                sid(f) == sid(&s)
                    && f.get("unit_id") == s.get("unit_id")
                    && f.get("ownership_epoch") == s.get("ownership_epoch")
                    && str_of(f, "owner") == Some(role)
            });
            let Some(mut fresh) = fresh else {
                return Err(refused(format!(
                    "POSTED_INCOMPLETE {}: the withdrawal note was posted, but the session, unit or ownership changed before the review could be cleared; nothing else was changed",
                    mid_of(&m)
                )));
            };
            if current_review(&fresh).and_then(|r| str_of(r, "request")) == Some(request.as_str()) {
                fresh.insert("review".into(), Value::Null);
            }
            raise_floor(&mut fresh, role, seq_of(&m));
            fresh.insert("updated_at".into(), json!(utc_now()));
            self.save(&fresh)?;
        }
        Ok(Out::ok(format!("REVIEW_WITHDRAWN request={request}\n")))
    }

    /// Peer `STEER`s in this unit that no later post by `owner` names, by
    /// `reply_to`, by id or by `#<seq>` (RS-5).
    pub(crate) fn unresolved_steers(&self, s: &Obj, owner: &str) -> Result<Vec<String>, MeshError> {
        let unit = s.get("unit_id");
        let mine: Vec<Obj> = jsonl::records(&self.mb.journal(sid(s), owner))?
            .into_iter()
            .filter(|m| m.get("unit_id") == unit)
            .collect();
        let mut out = Vec::new();
        for r in participants(s).into_iter().filter(|r| r != owner) {
            for m in jsonl::records(&self.mb.journal(sid(s), &r))? {
                if m.get("unit_id") != unit || str_of(&m, "kind") != Some("STEER") {
                    continue;
                }
                let id = mid_of(&m);
                let at = str_of(&m, "at").unwrap_or_default();
                let tag = format!("#{}", seq_of(&m));
                let named = mine.iter().any(|x| {
                    let body = str_of(x, "body").unwrap_or_default();
                    str_of(x, "at").unwrap_or_default() >= at
                        && (str_of(x, "reply_to") == Some(id.as_str())
                            || body.contains(&id)
                            || names_seq(body, &tag))
                });
                if !named {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    /// Where `msg_id`, a review request by `author` with sequence `seq`,
    /// stands against the published review, waiting up to `wait` for a
    /// publication still pending after its append.
    ///
    /// # Errors
    /// A refusal reading the session or a journal.
    pub(crate) fn review_publication(
        &self,
        role: &str,
        msg_id: &str,
        wait: Duration,
    ) -> Result<Publication, MeshError> {
        let end = Instant::now() + wait;
        loop {
            let s = self.require(role, true)?;
            let p = self.publication_of(&s, msg_id)?;
            if p != Publication::Pending || Instant::now() >= end {
                return Ok(p);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The ids of `author`'s review requests and withdrawals in this unit
    /// after `seq`, from its journal.
    fn later_mutations(&self, s: &Obj, author: &str, seq: i64) -> Result<Vec<String>, MeshError> {
        Ok(jsonl::records(&self.mb.journal(sid(s), author))?
            .into_iter()
            .filter(|m| {
                m.get("unit_id") == s.get("unit_id")
                    && seq_of(m) > seq
                    && (str_of(m, "kind") == Some("REVIEW_REQUEST")
                        || (str_of(m, "kind") == Some("NOTE")
                            && str_of(m, "body").is_some_and(|b| b.starts_with(WITHDRAWN))))
            })
            .map(|m| mid_of(&m))
            .collect())
    }

    /// The fingerprint list for the owner's own review request in a snapshot
    /// session (RS-1): every path the snapshot will publish, with the first 12
    /// hex characters of its exact-byte (or link) digest, `deleted` for a
    /// gone path. `None` outside a snapshot session; `Some(Err(why))` when
    /// nothing changed or a path cannot be reviewed as bytes.
    ///
    /// # Errors
    /// A refusal reading the tree.
    pub(crate) fn snapshot_manifest(&self, s: &Obj) -> Result<Option<Listing>, MeshError> {
        if !snapshot_on(s) {
            return Ok(None);
        }
        let trees = self.review_snapshot(s)?;
        let bad = problems(s, &trees);
        if !bad.is_empty() {
            return Ok(Some(Err(format!(
                "these paths cannot be reviewed as bytes: {}",
                bad.join(", ")
            ))));
        }
        let mut out = String::new();
        let mut n = 0;
        for (key, paths) in &trees {
            for (path, value) in paths {
                let shown = match value.as_str() {
                    "gone" => "deleted".to_owned(),
                    v => match v.strip_prefix("link:") {
                        Some(h) => format!("link:{}", &h[..h.len().min(12)]),
                        None => v[..v.len().min(12)].to_owned(),
                    },
                };
                out.push_str(&format!("{}={shown}\n", label(s, key, path)));
                n += 1;
            }
        }
        if n == 0 {
            return Ok(Some(Err(
                "nothing changed in the unit; there is nothing to review".into(),
            )));
        }
        Ok(Some(Ok((out, n))))
    }

    fn publication_of(&self, s: &Obj, msg_id: &str) -> Result<Publication, MeshError> {
        let rv = current_review(s);
        if let Some(rv) = rv.filter(|rv| str_of(rv, "request") == Some(msg_id)) {
            // The journal, not the published state, is what the owner said
            // last: a durable later request or withdrawal whose state update
            // was delayed or lost already retires this one.
            let author = str_of(rv, "role").unwrap_or_default();
            let rseq = rv.get("seq").and_then(Value::as_i64).unwrap_or(0);
            if !self.later_mutations(s, author, rseq)?.is_empty() {
                return Ok(Publication::Superseded);
            }
            return Ok(Publication::Current(trees_of(rv.get("trees"))));
        }
        let Some((author, seq)) = msg_id.split_once(':') else {
            return Ok(Publication::Superseded);
        };
        let seq: i64 = seq.parse().unwrap_or(0);
        let floor = s
            .get("review_floor")
            .and_then(Value::as_object)
            .and_then(|f| f.get(author))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if floor >= seq {
            return Ok(Publication::Superseded);
        }
        let later = !self.later_mutations(s, author, seq)?.is_empty();
        Ok(if later {
            Publication::Superseded
        } else {
            Publication::Pending
        })
    }

    /// Why the request `msg_id` cannot be agreed to as the bytes now stand,
    /// for a reviewer in a snapshot session: empty when it is the current
    /// review and the tree still matches its snapshot.
    ///
    /// # Errors
    /// A refusal reading the session, a journal or the tree.
    pub fn snapshot_review_problems(
        &self,
        role: &str,
        msg_id: &str,
        wait: Duration,
    ) -> Result<Vec<String>, MeshError> {
        let s = self.require(role, true)?;
        if !snapshot_on(&s) {
            return Ok(Vec::new());
        }
        match self.review_publication(role, msg_id, wait)? {
            Publication::Superseded => Ok(vec![format!(
                "{msg_id} is no longer the current review request: a newer request or a withdrawal replaced it"
            )]),
            Publication::Pending => Ok(vec![format!(
                "{msg_id} has no published review snapshot; post a new REVIEW_REQUEST"
            )]),
            Publication::Current(published) => {
                let s = self.require(role, true)?;
                let now = self.review_snapshot(&s)?;
                let mut out: Vec<String> = problems(&s, &now)
                    .into_iter()
                    .map(|p| format!("{p}: cannot be reviewed as bytes"))
                    .collect();
                out.extend(
                    changes(&s, &published, &now)
                        .into_iter()
                        .map(|p| format!("{p}: changed since the request's snapshot was published")),
                );
                Ok(out)
            }
        }
    }
    /// What a snapshot-session reviewer reviews (RS-4): the paths the
    /// current request published, verified as bytes by
    /// [`Self::snapshot_review_problems`], in place of the legacy textual
    /// manifest. `None` outside a snapshot session. A companion path is a
    /// problem: this reviewer reads only the anchor, and never agrees to
    /// bytes it cannot inspect.
    ///
    /// # Errors
    /// A refusal reading the session.
    pub fn snapshot_inventory(
        &self,
        role: &str,
        msg_id: &str,
    ) -> Result<Option<Inventory>, MeshError> {
        let s = self.require(role, true)?;
        if !snapshot_on(&s) {
            return Ok(None);
        }
        let Some(review) = current_review(&s).filter(|r| str_of(r, "request") == Some(msg_id))
        else {
            return Ok(Some(Err(vec![format!(
                "{msg_id} is not the current review request"
            )])));
        };
        let trees = trees_from(review.get("trees"));
        let mut files = Vec::new();
        let mut unseen = Vec::new();
        for (key, paths) in &trees {
            for path in paths.keys() {
                if key == "anchor" {
                    files.push(path.clone());
                } else {
                    unseen.push(format!(
                        "{}: in a companion repository this reviewer cannot inspect; ask a reviewer that can",
                        label(&s, key, path)
                    ));
                }
            }
        }
        Ok(Some(if unseen.is_empty() {
            Ok(files)
        } else {
            Err(unseen)
        }))
    }
}

/// Make the request `mid` the unit's current review, under the state lock,
/// after its append landed (RS-2). Never older than the current request or a
/// withdrawal: each role's floor only moves up.
pub(crate) fn publish_review(fresh: &mut Obj, role: &str, seq: i64, mid: &str, trees: &Trees) {
    let floor = fresh
        .get("review_floor")
        .and_then(Value::as_object)
        .and_then(|f| f.get(role))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let cur_newer = current_review(fresh).is_some_and(|c| {
        str_of(c, "role") == Some(role) && c.get("seq").and_then(Value::as_i64).unwrap_or(0) >= seq
    });
    if seq <= floor || cur_newer {
        return;
    }
    let unit = fresh.get("unit_id").cloned().unwrap_or(Value::Null);
    fresh.insert(
        "review".into(),
        json!({"request": mid, "role": role, "seq": seq, "unit_id": unit,
               "digest": digest(trees), "trees": trees}),
    );
    raise_floor(fresh, role, seq);
}

fn raise_floor(s: &mut Obj, role: &str, seq: i64) {
    let mut floor = s
        .get("review_floor")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let was = floor.get(role).and_then(Value::as_i64).unwrap_or(0);
    floor.insert(role.to_owned(), json!(was.max(seq)));
    s.insert("review_floor".into(), Value::Object(floor));
}

/// `REVIEW_PENDING` for the owner while a review is current (RS-5).
pub(crate) fn review_pending_line(s: &Obj) -> Option<String> {
    let rv = current_review(s)?;
    Some(format!(
        "REVIEW_PENDING request={} digest={}: an edit now changes the reviewed set; post a new REVIEW_REQUEST or run review-withdraw",
        str_of(rv, "request").unwrap_or_default(),
        str_of(rv, "digest").unwrap_or_default()
    ))
}

/// The `REVIEW_SNAPSHOT` status line (RS-6), for a session that uses them.
pub(crate) fn status_line(s: &Obj) -> Option<String> {
    if !snapshot_on(s) {
        return None;
    }
    Some(current_review(s).map_or_else(
        || "REVIEW_SNAPSHOT on".to_owned(),
        |rv| {
            format!(
                "REVIEW_SNAPSHOT on request={} digest={}",
                str_of(rv, "request").unwrap_or_default(),
                str_of(rv, "digest").unwrap_or_default()
            )
        },
    ))
}

fn mid_of(m: &Obj) -> String {
    str_of(m, "msg_id").map_or_else(
        || format!("{}:{}", str_of(m, "role").unwrap_or_default(), seq_of(m)),
        str::to_owned,
    )
}

/// Whether `body` names `#<seq>` as a whole token: not after a word
/// character or `:`, and not followed by a word character.
fn names_seq(body: &str, tag: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || c == '_';
    body.match_indices(tag).any(|(i, _)| {
        let before = body[..i].chars().next_back();
        let after = body[i + tag.len()..].chars().next();
        !before.is_some_and(|c| word(c) || c == ':') && !after.is_some_and(word)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn an_absolute_link_target_reads_as_python_reads_it_on_windows() {
        let read = |s: &str| reference_link_text(PathBuf::from(s));
        assert_eq!(read(r"C:\x\t.txt"), PathBuf::from(r"\\?\C:\x\t.txt"));
        assert_eq!(
            read(r"\\srv\share\t.txt"),
            PathBuf::from(r"\\?\UNC\srv\share\t.txt")
        );
        assert_eq!(read(r"\\?\C:\x"), PathBuf::from(r"\\?\C:\x"));
        assert_eq!(read("t.txt"), PathBuf::from("t.txt"));
        assert_eq!(read(r"..\t.txt"), PathBuf::from(r"..\t.txt"));
    }

    #[test]
    fn the_digest_hashes_python_compatible_ascii_json() {
        let mut t = Trees::new();
        let mut a = BTreeMap::new();
        a.insert("b.txt".to_owned(), "gone".to_owned());
        a.insert(
            "\u{e9}/\u{1d4b3}.txt".to_owned(),
            "x\"\\\n\u{7f}".to_owned(),
        );
        t.insert("anchor".into(), a);
        t.insert("c0".into(), BTreeMap::new());
        assert_eq!(
            canonical(&t),
            r#"{"anchor":{"b.txt":"gone","\u00e9/\ud835\udcb3.txt":"x\"\\\n\u007f"},"c0":{}}"#
        );
        // json.dumps({"anchor": {}}, sort_keys=True, separators=(",", ":"))
        let mut e = Trees::new();
        e.insert("anchor".into(), BTreeMap::new());
        assert_eq!(
            digest(&e),
            format!("S:{}", &hex(&Sha256::digest(br#"{"anchor":{}}"#))[..12])
        );
    }

    #[test]
    fn a_request_is_published_only_above_the_floor() {
        let mut s = Obj::new();
        s.insert("unit_id".into(), json!("1-a"));
        let t = Trees::new();
        publish_review(&mut s, "claude", 3, "claude:3", &t);
        assert_eq!(
            current_review(&s).and_then(|r| str_of(r, "request")),
            Some("claude:3")
        );
        publish_review(&mut s, "claude", 2, "claude:2", &t);
        assert_eq!(
            current_review(&s).and_then(|r| str_of(r, "request")),
            Some("claude:3")
        );
        s.insert("review".into(), Value::Null);
        raise_floor(&mut s, "claude", 5);
        publish_review(&mut s, "claude", 4, "claude:4", &t);
        assert!(current_review(&s).is_none());
        publish_review(&mut s, "claude", 6, "claude:6", &t);
        assert_eq!(
            current_review(&s).and_then(|r| str_of(r, "request")),
            Some("claude:6")
        );
    }

    #[test]
    fn a_steer_is_named_by_its_seq_only_as_a_whole_token() {
        assert!(names_seq("fixed #4 now", "#4"));
        assert!(names_seq("#4", "#4"));
        assert!(!names_seq("fixed #42", "#4"));
        assert!(!names_seq("codex:#4", "#4"));
        assert!(!names_seq("a#4", "#4"));
    }

    #[test]
    fn entry_values_name_what_cannot_be_read_as_bytes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"abc").unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        assert_eq!(
            entry_value(dir.path(), "f.txt"),
            hex(&Sha256::digest(b"abc"))
        );
        assert_eq!(entry_value(dir.path(), "missing/x.txt"), "gone");
        assert_eq!(entry_value(dir.path(), "d"), "unsupported");
        assert_eq!(entry_value(dir.path(), "../outside.txt"), "unsupported");
    }
}
