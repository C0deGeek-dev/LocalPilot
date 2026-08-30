//! What the working tree *is*, right now, as one comparable value.
//!
//! A commit hash answers "which commit" but not "which bytes": an uncommitted
//! edit, a staged hunk, and a stray new file all produce a different binary from
//! the same `HEAD`. Every later step in this crate — build dedup, staleness
//! detection, refusing to publish a stale binary — needs the second answer, so
//! it is computed once here and carried as a [`SourceState`].
//!
//! The fingerprint is a SHA-256 over a **framed** digest: each section is
//! written as a label, a byte length, then the bytes. Framing is what makes the
//! digest injective — without it a file named `b` containing `c` and a file
//! named `bc` containing nothing would hash identically.
//!
//! Untracked files are hashed by content, not merely by name, because an
//! untracked file is exactly the case a commit hash cannot see at all. They are
//! read in chunks, so an oversized stray file costs time rather than memory.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::SelfDevError;
use crate::git::git;

/// How many hex characters of the commit hash go into a version label.
const SHORT_HASH_LEN: usize = 7;
/// How many hex characters of the fingerprint go into a dirty version label.
/// Long enough that two working trees in one session will not collide, short
/// enough to stay readable in a directory name.
const LABEL_FINGERPRINT_LEN: usize = 12;
/// Stand-in short hash for a repository with no commits yet.
const NO_COMMIT_SHORT: &str = "0000000";
/// Read size for hashing untracked file contents.
const CHUNK: usize = 64 * 1024;

/// A fingerprint of one working tree at one instant.
///
/// Two reads of the same bytes produce the same [`SourceState::version_label`];
/// changing a single byte of any tracked *or* untracked file changes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceState {
    /// The working tree's root, as git reports it.
    pub root: PathBuf,
    /// The commit `HEAD` points at, full length. `None` in a repository with no
    /// commits yet.
    pub head: Option<String>,
    /// Whether the tree differs from `HEAD` in any way — tracked modifications,
    /// staged changes, or untracked files.
    pub dirty: bool,
    /// SHA-256, hex, over the framed digest of head + status + diff + untracked
    /// contents.
    pub fingerprint: String,
    /// A stable, filesystem-safe name for this exact tree: `<short-hash>` when
    /// clean, `<short-hash>-dirty-<fingerprint-prefix>` when not.
    pub version_label: String,
    /// Paths that differ from `HEAD`, sorted. For a rename, the destination.
    pub changed_paths: Vec<String>,
}

impl SourceState {
    /// Read the state of the working tree containing `dir`.
    ///
    /// # Errors
    /// Returns [`SelfDevError::NotAWorkingTree`] when `dir` is not inside a git
    /// working tree, and [`SelfDevError::Git`] when a git invocation fails for
    /// any other reason.
    pub fn read(dir: &Path) -> Result<Self, SelfDevError> {
        let root = match git(dir, &["rev-parse", "--show-toplevel"]) {
            Ok(output) => PathBuf::from(output.trim()),
            Err(_) => return Err(SelfDevError::NotAWorkingTree(dir.display().to_string())),
        };

        // A repository with no commits has no HEAD to resolve. That is a
        // legitimate state (a freshly `git init`ed tree), not a failure, so it
        // becomes `None` rather than an error.
        let head = git(&root, &["rev-parse", "HEAD"])
            .ok()
            .map(|output| output.trim().to_string())
            .filter(|hash| !hash.is_empty());

        let status = git(
            &root,
            &["status", "--porcelain=v1", "--untracked-files=all"],
        )?;
        // Without a HEAD there is nothing to diff against; the staged content is
        // still visible through status and the untracked scan.
        let diff = if head.is_some() {
            git(&root, &["diff", "HEAD"])?
        } else {
            String::new()
        };
        let mut untracked = git(&root, &["ls-files", "--others", "--exclude-standard"])?
            .lines()
            .map(str::to_string)
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>();
        untracked.sort();

        let dirty = !status.trim().is_empty();
        let fingerprint = fingerprint(&root, head.as_deref(), &status, &diff, &untracked)?;
        let short = head
            .as_deref()
            .map_or(NO_COMMIT_SHORT.to_string(), short_hash);
        let version_label = if dirty {
            format!(
                "{short}-dirty-{}",
                &fingerprint[..LABEL_FINGERPRINT_LEN.min(fingerprint.len())]
            )
        } else {
            short
        };

        Ok(SourceState {
            root,
            head,
            dirty,
            fingerprint,
            version_label,
            changed_paths: changed_paths(&status),
        })
    }

    /// The commit hash to embed in a binary built from this tree, or `unknown`
    /// when the tree has no commits.
    #[must_use]
    pub fn embedded_hash(&self) -> &str {
        self.head.as_deref().unwrap_or("unknown")
    }
}

/// The leading characters of a commit hash used in a version label.
fn short_hash(hash: &str) -> String {
    hash.chars().take(SHORT_HASH_LEN).collect()
}

/// SHA-256 over the framed digest of everything that can change the build.
fn fingerprint(
    root: &Path,
    head: Option<&str>,
    status: &str,
    diff: &str,
    untracked: &[String],
) -> Result<String, SelfDevError> {
    let mut hasher = Sha256::new();
    frame(&mut hasher, "head", head.unwrap_or("").as_bytes());
    frame(&mut hasher, "status", status.as_bytes());
    frame(&mut hasher, "diff", diff.as_bytes());
    for path in untracked {
        frame(&mut hasher, "untracked-path", path.as_bytes());
        hash_file(&mut hasher, &root.join(path))?;
    }
    Ok(hex(&hasher.finalize()))
}

/// Feed one labelled, length-prefixed section into the digest, so no two
/// different inputs can produce the same byte stream.
fn frame(hasher: &mut Sha256, label: &str, bytes: &[u8]) {
    hasher.update(label.as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes.len().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(bytes);
}

/// Feed a file's contents into the digest in chunks.
///
/// A file listed as untracked but unreadable (a dangling symlink, a permission
/// wall, a file deleted between the listing and the read) is folded in as an
/// explicit `unreadable` marker rather than skipped: skipping would let an
/// unreadable file and no file at all fingerprint identically.
fn hash_file(hasher: &mut Sha256, path: &Path) -> Result<(), SelfDevError> {
    use std::io::Read;

    let Ok(mut file) = std::fs::File::open(path) else {
        frame(hasher, "untracked-body", b"<unreadable>");
        return Ok(());
    };
    let length = file.metadata().map_err(SelfDevError::io)?.len();
    hasher.update(b"untracked-body\0");
    hasher.update(length.to_string().as_bytes());
    hasher.update(b"\0");
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => hasher.update(&buffer[..read]),
            Err(error) => return Err(SelfDevError::io(error)),
        }
    }
    Ok(())
}

/// Lower-case hex of a digest.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Paths named by a `--porcelain=v1` status listing, sorted and de-duplicated.
///
/// Each line is two status columns, a space, then the path; a rename carries
/// `from -> to`, and the destination is the path that now exists, so that is the
/// one recorded. Paths containing unusual bytes are quoted by git, and the
/// quotes are stripped for readability — this list is for humans and for
/// "did my build go stale", never for reopening the file.
fn changed_paths(status: &str) -> Vec<String> {
    let mut paths: Vec<String> = status
        .lines()
        .filter_map(|line| {
            if line.len() < 4 {
                return None;
            }
            let path = line[3..].trim();
            let path = path.rsplit(" -> ").next().unwrap_or(path);
            let path = path.trim_matches('"');
            (!path.is_empty()).then(|| path.to_string())
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit_all, init_repo, write};

    #[test]
    fn a_clean_tree_is_labelled_by_its_short_commit_hash() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");

        let state = SourceState::read(repo.path()).expect("read");

        assert!(!state.dirty, "a committed tree is clean");
        let head = state.head.clone().expect("a committed tree has a HEAD");
        assert_eq!(state.version_label, head[..SHORT_HASH_LEN]);
        assert!(state.changed_paths.is_empty());
    }

    #[test]
    fn identical_trees_produce_identical_labels() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");

        let first = SourceState::read(repo.path()).expect("first read");
        let second = SourceState::read(repo.path()).expect("second read");

        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.version_label, second.version_label);
    }

    #[test]
    fn a_one_byte_untracked_change_changes_the_dirty_fingerprint() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");

        write(repo.path(), "stray.txt", "x");
        let before = SourceState::read(repo.path()).expect("before");
        write(repo.path(), "stray.txt", "y");
        let after = SourceState::read(repo.path()).expect("after");

        assert!(before.dirty && after.dirty, "an untracked file is dirt");
        assert_ne!(
            before.fingerprint, after.fingerprint,
            "one byte of untracked content must move the fingerprint"
        );
        assert_ne!(before.version_label, after.version_label);
        assert!(before.version_label.contains("-dirty-"));
    }

    #[test]
    fn a_tracked_edit_changes_the_fingerprint_and_is_reported_as_changed() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");
        let clean = SourceState::read(repo.path()).expect("clean");

        write(repo.path(), "a.txt", "two");
        let edited = SourceState::read(repo.path()).expect("edited");

        assert!(!clean.dirty && edited.dirty);
        assert_ne!(clean.fingerprint, edited.fingerprint);
        assert_eq!(edited.changed_paths, vec!["a.txt".to_string()]);
        assert_eq!(
            clean.head, edited.head,
            "the commit did not move, only the bytes did"
        );
    }

    #[test]
    fn adding_and_removing_an_untracked_file_returns_to_the_original_label() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");
        let before = SourceState::read(repo.path()).expect("before");

        write(repo.path(), "stray.txt", "x");
        std::fs::remove_file(repo.path().join("stray.txt")).expect("remove");
        let after = SourceState::read(repo.path()).expect("after");

        assert_eq!(
            before.version_label, after.version_label,
            "returning the tree to its earlier bytes must return the label"
        );
    }

    #[test]
    fn a_repository_with_no_commits_still_yields_a_label() {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");

        let state = SourceState::read(repo.path()).expect("read");

        assert_eq!(state.head, None);
        assert!(state.dirty);
        assert!(state.version_label.starts_with(NO_COMMIT_SHORT));
        assert_eq!(state.embedded_hash(), "unknown");
    }

    #[test]
    fn reading_outside_a_working_tree_is_a_typed_error() {
        let outside = tempfile::tempdir().expect("temp");
        // Stop git's walk-up at the temp directory, so a repository that happens
        // to sit above the system temp root cannot make this pass or fail by
        // accident.
        let previous = std::env::var_os("GIT_CEILING_DIRECTORIES");
        std::env::set_var("GIT_CEILING_DIRECTORIES", outside.path());
        let result = SourceState::read(outside.path());
        match previous {
            Some(value) => std::env::set_var("GIT_CEILING_DIRECTORIES", value),
            None => std::env::remove_var("GIT_CEILING_DIRECTORIES"),
        }

        assert!(matches!(result, Err(SelfDevError::NotAWorkingTree(_))));
    }

    #[test]
    fn framing_distinguishes_inputs_a_plain_concatenation_would_not() {
        let mut joined = Sha256::new();
        frame(&mut joined, "s", b"bc");
        frame(&mut joined, "s", b"");
        let first = hex(&joined.finalize());

        let mut split = Sha256::new();
        frame(&mut split, "s", b"b");
        frame(&mut split, "s", b"c");
        let second = hex(&split.finalize());

        assert_ne!(
            first, second,
            "length framing must keep the digest injective"
        );
    }

    #[test]
    fn a_rename_is_reported_by_its_destination() {
        let listing = "R  old.txt -> new.txt\n M other.txt\n";
        assert_eq!(
            changed_paths(listing),
            vec!["new.txt".to_string(), "other.txt".to_string()]
        );
    }
}
