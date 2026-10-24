//! Static source proof. Reads Git metadata and contained ordinary files only;
//! never probes a proposed executable, imports modules or starts a build.

use super::{
    CanonicalHash, ContentDigest, ProposalIdentity, RepositoryTransaction, ValidationError,
    ValidationPlanIdentity,
};
use crate::{git, ChangeProvenance, DiffSummary, ProposedPatch, PROVENANCE_SCHEMA};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const MAX_SOURCE_FILES: usize = 20_000;
const MAX_SOURCE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SUBMODULE_DEPTH: usize = 8;

/// Proposal-owned static report. Commands/profiles are resolved by the host;
/// this record never constitutes consent or passing validation evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalInspection {
    /// Exact source identity for subsequent plan/consent binding.
    pub identity: ProposalIdentity,
    /// Existing bounded review surface, not a second diff representation.
    pub diff: DiffSummary,
    /// Files in the full Git diff whose numstat is binary.
    pub binary_paths: Vec<String>,
    /// Number of ordinary source files inspected, including submodules.
    pub source_files: usize,
    /// Source bytes hashed, excluding explicitly declared caches/artifacts.
    pub source_bytes: u64,
}

impl ProposedPatch {
    /// Read a bounded ordinary text file from the recorded base, never from the
    /// proposal's replacement configuration. Missing files remain missing.
    ///
    /// # Errors
    /// Unsupported path/mode, invalid UTF8, Git failure or text larger than64KiB.
    pub fn validation_base_text(&self, path: &str) -> Result<Option<String>, ValidationError> {
        if !supported_path(path) || !object_id(&self.base_commit) {
            return Err(invalid("unsupported base configuration path"));
        }
        let entry = git::git(
            &self.repo_root,
            &["ls-tree", "-z", &self.base_commit, "--", path],
        )?;
        if entry.is_empty() {
            return Ok(None);
        }
        if !entry.starts_with("100644 blob ") && !entry.starts_with("100755 blob ") {
            return Err(invalid(
                "base configuration is not an ordinary tracked file",
            ));
        }
        let value = git::git(
            &self.repo_root,
            &[
                "show",
                "--no-ext-diff",
                "--no-textconv",
                &format!("{}:{path}", self.base_commit),
            ],
        )?;
        if value.len() > 64 * 1024 {
            return Err(invalid("base configuration exceeds64KiB"));
        }
        Ok(Some(value))
    }

    /// Inspect current proposal, target, scope and source bytes under the common
    /// repository lock. The host must resolve the trusted command plan separately.
    ///
    /// # Errors
    /// Refuses dirty/missing/aliased roots, unsupported paths/text policy,
    /// submodule changes or uninitialized submodules, unapproved ignored output,
    /// missing origin metadata, widened scope and bounded-source exhaustion.
    pub fn inspect_validation(
        &self,
        transaction: &RepositoryTransaction<'_>,
        plan: &ValidationPlanIdentity,
    ) -> Result<ProposalInspection, ValidationError> {
        plan.digest()?;
        transaction.ensure_repository(&self.repo_root)?;
        transaction.ensure_repository(self.worktree.path())?;
        let root = canonical_root(&self.repo_root)?;
        let worktree = canonical_root(self.worktree.path())?;
        let planned =
            fs::canonicalize(&plan.worktree).map_err(|source| io(&plan.worktree, source))?;
        if planned != worktree || !worktree.starts_with(root.join(git::WORKTREES_DIR)) {
            return Err(ValidationError::Stale);
        }
        let base = self.provenance.proposal_base.as_ref().ok_or_else(|| {
            invalid("legacy proposal has no recorded origin/scope; regenerate it before validation")
        })?;
        if self.provenance.schema != PROVENANCE_SCHEMA
            || base.commit != self.base_commit
            || !object_id(&base.commit)
            || !base.target_ref.starts_with("refs/heads/")
            || base.allowed_paths.iter().any(|path| !supported_path(path))
        {
            return Err(invalid("unsupported proposal origin"));
        }
        if git::git(&root, &["symbolic-ref", "--quiet", "HEAD"])?.trim() != base.target_ref
            || git::head_commit(&root)? != base.commit
            || !git::is_clean(&root)?
            || !git::is_clean(&worktree)?
        {
            return Err(ValidationError::Stale);
        }
        let branch = format!("refs/heads/{}", self.id);
        if git::git(&worktree, &["symbolic-ref", "--quiet", "HEAD"])?.trim() != branch {
            return Err(ValidationError::Stale);
        }
        let proposed_commit = git::head_commit(&worktree)?;
        if git::git(&root, &["rev-parse", "--verify", &branch])?.trim() != proposed_commit {
            return Err(ValidationError::Stale);
        }
        git::git(
            &root,
            &[
                "merge-base",
                "--is-ancestor",
                &base.commit,
                &proposed_commit,
            ],
        )?;
        let proposed_tree = git::git(&worktree, &["rev-parse", "HEAD^{tree}"])?
            .trim()
            .to_owned();
        if !object_id(&proposed_commit) || !object_id(&proposed_tree) {
            return Err(invalid("invalid proposed object identity"));
        }
        let changed = git::git(
            &worktree,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                &base.commit,
                &proposed_commit,
            ],
        )?;
        let changed: Vec<_> = changed.split('\0').filter(|p| !p.is_empty()).collect();
        if changed.is_empty()
            || changed.iter().any(|path| {
                !supported_path(path) || !base.allowed_paths.iter().any(|allowed| allowed == path)
            })
        {
            return Err(invalid("proposal changed outside its recorded scope"));
        }
        // Changes to gitlinks are unsupported even when the resulting submodule
        // happens to be initialized. Unchanged, clean pinned submodules are read.
        let raw_diff = git::git(
            &worktree,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--raw",
                "--no-abbrev",
                &base.commit,
                &proposed_commit,
            ],
        )?;
        if raw_diff.lines().any(|line| {
            line.starts_with(":160000 ") || line.split_whitespace().nth(1) == Some("160000")
        }) {
            return Err(invalid("submodule changes need separate review"));
        }
        let mut source = SourceProof {
            hash: CanonicalHash::new("localpilot-source-bytes-v1"),
            files: 0,
            bytes: 0,
        };
        hash_sources(&worktree, "", 0, plan, &mut source)?;
        let source_bytes = source.hash.finish();
        let policy_digest = text_policy(&worktree)?;
        let full_diff = git::git(
            &worktree,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--binary",
                &base.commit,
                &proposed_commit,
            ],
        )?;
        let diff = ContentDigest::hash("localpilot-full-proposal-diff-v1", &[full_diff.as_bytes()]);
        let binary_paths = git::git(
            &worktree,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--numstat",
                "-z",
                &base.commit,
                &proposed_commit,
            ],
        )?
        .split('\0')
        .filter_map(|row| row.strip_prefix("-\t-\t").map(str::to_owned))
        .collect();
        let mut final_source = SourceProof {
            hash: CanonicalHash::new("localpilot-source-bytes-v1"),
            files: 0,
            bytes: 0,
        };
        hash_sources(&worktree, "", 0, plan, &mut final_source)?;
        if final_source.hash.finish() != source_bytes || text_policy(&worktree)? != policy_digest {
            return Err(ValidationError::Stale);
        }
        // Check identities again around filesystem reads; a changed source must
        // not be silently associated with the earlier commit/root snapshot.
        if git::head_commit(&root)? != base.commit
            || git::head_commit(&worktree)? != proposed_commit
            || !git::is_clean(&root)?
            || !git::is_clean(&worktree)?
        {
            return Err(ValidationError::Stale);
        }
        Ok(ProposalInspection {
            identity: ProposalIdentity {
                repository: transaction.repository_identity(),
                proposal_id: self.id.clone(),
                target_ref: base.target_ref.clone(),
                base_commit: base.commit.clone(),
                proposed_commit,
                proposed_tree,
                source_bytes,
                diff,
                provenance: provenance_digest(&self.provenance),
                text_policy: policy_digest,
            },
            diff: crate::compute_diff_summary(&worktree, &base.commit)?,
            binary_paths,
            source_files: source.files,
            source_bytes: source.bytes,
        })
    }
}

struct SourceProof {
    hash: CanonicalHash,
    files: usize,
    bytes: u64,
}

fn hash_sources(
    root: &Path,
    prefix: &str,
    depth: usize,
    plan: &ValidationPlanIdentity,
    proof: &mut SourceProof,
) -> Result<(), ValidationError> {
    canonical_root(root)?;
    if depth > MAX_SUBMODULE_DEPTH || !git::is_clean(root)? {
        return Err(invalid("unsupported or dirty submodule source"));
    }
    let ignored = git::git(
        root,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
    )?;
    for path in ignored.split('\0').filter(|p| !p.is_empty()) {
        let relative = format!("{prefix}{path}");
        if !plan.artifact_paths.contains(&relative)
            && !plan
                .cache_paths
                .iter()
                .any(|cache| relative.starts_with(&format!("{cache}/")))
        {
            return Err(invalid(
                "ignored source/output outside declared artifact/cache paths",
            ));
        }
    }
    let files = git::git(root, &["ls-files", "--stage", "-z"])?;
    let paths = git::git(root, &["ls-files", "-z"])?;
    let attributes = git::attribute_output(root, paths.as_bytes(), None)?;
    let attribute_fields: Vec<_> = attributes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .collect();
    for row in attribute_fields.chunks(3) {
        if row.len() != 3 || matches!(row[1], b"filter" | b"working-tree-encoding" | b"ident") {
            return Err(invalid("unsupported source text transformation"));
        }
    }
    proof.hash.field(&attributes);
    let policy = text_policy(root)?;
    proof.hash.field(policy.as_str().as_bytes());
    for row in files.split('\0').filter(|row| !row.is_empty()) {
        let (metadata, path) = row
            .split_once('\t')
            .ok_or_else(|| invalid("malformed source index"))?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if fields.len() != 3 || fields[2] != "0" || !object_id(fields[1]) || !supported_path(path) {
            return Err(invalid("unsupported source path/index stage"));
        }
        let target = git::contained_edit_path(root, path)?;
        let relative = format!("{prefix}{path}");
        proof.hash.field(relative.as_bytes());
        proof.hash.field(metadata.as_bytes());
        match fields[0] {
            "160000" => {
                if git::head_commit(&target)? != fields[1] {
                    return Err(invalid("uninitialized or changed submodule"));
                }
                hash_sources(&target, &format!("{relative}/"), depth + 1, plan, proof)?;
            }
            "100644" | "100755" => {
                let metadata = fs::symlink_metadata(&target).map_err(|e| io(&target, e))?;
                if !metadata.file_type().is_file()
                    || metadata.len() > MAX_FILE_BYTES
                    || proof.files >= MAX_SOURCE_FILES
                {
                    return Err(invalid("source is not a bounded ordinary file"));
                }
                let mut bytes = Vec::new();
                fs::File::open(&target)
                    .map_err(|e| io(&target, e))?
                    .take(MAX_FILE_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| io(&target, e))?;
                proof.bytes = proof
                    .bytes
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| invalid("source size overflow"))?;
                if bytes.len() as u64 > MAX_FILE_BYTES || proof.bytes > MAX_SOURCE_BYTES {
                    return Err(invalid("source proof bounds exceeded"));
                }
                proof.files += 1;
                proof.hash.field(&bytes);
            }
            _ => {
                return Err(invalid(
                    "symlink or unsupported Git mode in validation source",
                ))
            }
        }
    }
    Ok(())
}

fn canonical_root(root: &Path) -> Result<PathBuf, ValidationError> {
    let canonical = fs::canonicalize(root).map_err(|e| io(root, e))?;
    let top = git::git(&canonical, &["rev-parse", "--show-toplevel"])?;
    if fs::canonicalize(top.trim()).map_err(|e| io(root, e))? != canonical {
        return Err(invalid("missing or aliased Git worktree root"));
    }
    Ok(canonical)
}

fn text_policy(root: &Path) -> Result<ContentDigest, ValidationError> {
    // Git config is read as names/values for text-only settings; no helper
    // command value or secret enters this identity or a public record.
    let mut hash = CanonicalHash::new("localpilot-git-text-policy-v1");
    for name in [
        "core.autocrlf",
        "core.eol",
        "core.attributesFile",
        "core.ignorecase",
        "core.filemode",
    ] {
        let value = git::config_value(root, name)?;
        hash.field(name.as_bytes());
        match value {
            Some(value) => {
                hash.field(b"present");
                hash.field(value.as_bytes());
            }
            None => hash.field(b"absent"),
        }
    }
    Ok(hash.finish())
}

fn provenance_digest(record: &ChangeProvenance) -> ContentDigest {
    let mut hash = CanonicalHash::new("localpilot-scoped-provenance-v1");
    for value in [
        &record.schema,
        &record.prompt,
        &record.model,
        &record.test_evidence,
        &record.rationale,
        &record.risks,
        &record.rollback_notes,
    ] {
        hash.field(value.as_bytes());
    }
    for list in [&record.tools_used, &record.lessons] {
        hash.number(list.len() as u64);
        for value in list {
            hash.field(value.as_bytes());
        }
    }
    if let Some(base) = &record.proposal_base {
        hash.number(base.allowed_paths.len() as u64);
        for path in &base.allowed_paths {
            hash.field(path.as_bytes());
        }
    }
    // Validation/legacy eval booleans do not authenticate or recursively hash a
    // receipt. Current command observations are verified independently.
    hash.finish()
}

fn object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn supported_path(path: &str) -> bool {
    super::valid_relative_path(path)
        && !path.chars().any(char::is_control)
        && !matches!(path.split('/').next(), Some(".git" | ".localpilot"))
}

fn invalid(message: &str) -> ValidationError {
    ValidationError::Invalid(message.into())
}
fn io(path: impl AsRef<Path>, source: std::io::Error) -> ValidationError {
    ValidationError::Io {
        path: path.as_ref().to_owned(),
        source,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::validation::{tests::fixture, RepositoryAuthority};

    fn proposal_fixture() -> (tempfile::TempDir, ProposedPatch) {
        let repo = tempfile::tempdir().unwrap();
        let root = repo.path();
        git::git(root, &["init", "-b", "main"]).unwrap();
        git::git(root, &["config", "user.name", "fixture"]).unwrap();
        git::git(root, &["config", "user.email", "fixture@localhost"]).unwrap();
        fs::write(root.join(".gitignore"), ".localpilot/\ntarget/\n").unwrap();
        fs::write(root.join(".gitattributes"), "sample.py text\n").unwrap();
        fs::write(root.join("sample.py"), "def answer():\n    return 1\n").unwrap();
        git::git(root, &["add", "."]).unwrap();
        git::git(root, &["commit", "-m", "controlled fixture"]).unwrap();
        let request = crate::PatchProposal::new(
            "controlled one-file proposal",
            vec!["sample.py".into()],
            vec![crate::ProposedEdit::new(
                "sample.py",
                "def answer():\n    return 2\n",
            )],
        );
        let patch = crate::propose(
            root,
            "inspect-fixture",
            &request,
            ChangeProvenance::new("fixture", "scripted", "inspect without executing"),
        )
        .unwrap();
        (repo, patch)
    }

    #[test]
    fn actual_proposal_inspection_is_repeatable_and_never_executes_the_proposed_module() {
        let (repo, patch) = proposal_fixture();
        let original = git::head_commit(repo.path()).unwrap();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = store.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        let first = patch.inspect_validation(&tx, &plan).unwrap();
        let again = patch.inspect_validation(&tx, &plan).unwrap();
        assert_eq!(first, again);
        assert_eq!(first.identity.base_commit, original);
        assert_eq!(first.diff.files, ["sample.py"]);
        assert!(first.diff.patch.contains("+    return 2"));
        assert_eq!(git::head_commit(repo.path()).unwrap(), original);
        assert!(patch.provenance().validation.is_none());
        assert_eq!(
            tx.events().count(),
            0,
            "inspection never consumes or fabricates execution consent"
        );
    }

    #[test]
    fn dirty_untracked_and_unapproved_ignored_sources_refuse_static_identity() {
        let (repo, patch) = proposal_fixture();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = store.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        let source = patch.worktree_path().join("sample.py");
        let original = fs::read(&source).unwrap();
        fs::write(&source, "changed without commit").unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
        fs::write(&source, original).unwrap();
        let untracked = patch.worktree_path().join("extra.py");
        fs::write(&untracked, "raise RuntimeError('must never be imported')\n").unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
        fs::remove_file(untracked).unwrap();
        let cache = patch.worktree_path().join("target");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("cache.bin"), "declared build cache").unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
        plan.cache_paths = vec!["target".into()];
        assert!(patch.inspect_validation(&tx, &plan).is_ok());
    }

    #[test]
    fn origin_survives_additional_commits_but_scope_cannot_expand() {
        let (repo, patch) = proposal_fixture();
        let original_base = patch.base_commit().to_owned();
        let id = patch.id().to_owned();
        let worktree = patch.worktree_path().to_owned();
        patch.persist().unwrap();
        fs::write(worktree.join("sample.py"), "def answer():\n    return 3\n").unwrap();
        git::git(&worktree, &["add", "."]).unwrap();
        git::git(
            &worktree,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "-m",
                "controlled revision",
            ],
        )
        .unwrap();
        let patch = ProposedPatch::reopen(repo.path(), &id).unwrap();
        assert_eq!(patch.base_commit(), original_base);
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = store.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = worktree.to_str().unwrap().into();
        assert_eq!(
            patch
                .inspect_validation(&tx, &plan)
                .unwrap()
                .identity
                .base_commit,
            original_base
        );
        fs::write(worktree.join("outside-scope.py"), "print('must not run')\n").unwrap();
        git::git(&worktree, &["add", "."]).unwrap();
        git::git(
            &worktree,
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@localhost",
                "commit",
                "-m",
                "controlled out of scope revision",
            ],
        )
        .unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
    }

    #[test]
    fn changed_target_and_legacy_missing_origin_never_gain_fabricated_evidence() {
        let (repo, mut patch) = proposal_fixture();
        let mut store = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = store.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        patch.provenance.proposal_base = None;
        assert!(patch.inspect_validation(&tx, &plan).is_err());
        assert!(patch.provenance().validation.is_none());
        patch.provenance.proposal_base = Some(crate::ProposalBase {
            commit: patch.base_commit().into(),
            target_ref: "refs/heads/main".into(),
            allowed_paths: vec!["sample.py".into()],
        });
        git::git(
            repo.path(),
            &["commit", "--allow-empty", "-m", "target advanced"],
        )
        .unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
    }

    #[test]
    fn raw_checkout_line_endings_change_identity_even_when_git_considers_them_clean() {
        let (repo, patch) = proposal_fixture();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = authority.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        let before = patch.inspect_validation(&tx, &plan).unwrap();
        let source = patch.worktree_path().join("sample.py");
        let text = fs::read_to_string(&source).unwrap().replace("\r\n", "\n");
        fs::write(&source, text.replace('\n', "\r\n")).unwrap();
        // Refresh the fixture index after normalization: some Git versions
        // retain an unstaged stat change despite an empty normalized diff.
        // This writes no source/commit and leaves the exact tree unchanged.
        git::git(patch.worktree_path(), &["add", "sample.py"]).unwrap();
        assert!(
            git::is_clean(patch.worktree_path()).unwrap(),
            "{}",
            git::git(patch.worktree_path(), &["status", "--porcelain"]).unwrap()
        );
        let after = patch.inspect_validation(&tx, &plan).unwrap();
        if fs::read(&source).unwrap() == text.as_bytes() {
            panic!("fixture did not change checkout bytes");
        }
        assert_ne!(before.identity.source_bytes, after.identity.source_bytes);
    }

    #[test]
    fn binary_and_option_looking_filenames_are_inspected_as_source_not_git_options() {
        let (repo, patch) = proposal_fixture();
        patch.discard().unwrap();
        let name = "--config=controlled.bin";
        let request = crate::PatchProposal::new(
            "controlled binary filename",
            vec![name.into()],
            vec![crate::ProposedEdit::new(name, "binary\0payload")],
        );
        let patch = crate::propose(
            repo.path(),
            "binary-fixture",
            &request,
            ChangeProvenance::new("fixture", "scripted", "binary static inspection"),
        )
        .unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = authority.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        let report = patch.inspect_validation(&tx, &plan).unwrap();
        assert_eq!(report.binary_paths, [name]);
        assert_eq!(report.diff.files, [name]);
        assert_eq!(tx.events().count(), 0);
        assert!(!repo.path().join(name).exists());
    }

    #[test]
    fn unchanged_initialized_submodules_are_bound_and_uninitialized_or_dirty_ones_refuse() {
        let (repo, patch) = proposal_fixture();
        patch.discard().unwrap();
        let module = tempfile::tempdir().unwrap();
        git::git(module.path(), &["init", "-b", "main"]).unwrap();
        git::git(module.path(), &["config", "user.name", "fixture"]).unwrap();
        git::git(
            module.path(),
            &["config", "user.email", "fixture@localhost"],
        )
        .unwrap();
        fs::write(module.path().join("module.txt"), "pinned module source\n").unwrap();
        git::git(module.path(), &["add", "."]).unwrap();
        git::git(
            module.path(),
            &["commit", "-m", "controlled module fixture"],
        )
        .unwrap();
        let raw = |root: &Path, args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(root)
                .args([
                    "-c",
                    "protocol.file.allow=always",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "commit.gpgSign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        raw(
            repo.path(),
            &[
                "submodule",
                "add",
                module.path().to_str().unwrap(),
                "deps/example",
            ],
        );
        git::git(repo.path(), &["add", ".gitmodules", "deps/example"]).unwrap();
        git::git(
            repo.path(),
            &["commit", "-m", "controlled unchanged submodule base"],
        )
        .unwrap();
        let request = crate::PatchProposal::new(
            "controlled one-file proposal",
            vec!["sample.py".into()],
            vec![crate::ProposedEdit::new(
                "sample.py",
                "def answer():\n    return 2\n",
            )],
        );
        let patch = crate::propose(
            repo.path(),
            "submodule-fixture",
            &request,
            ChangeProvenance::new("fixture", "scripted", "pinned submodule identity"),
        )
        .unwrap();
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = authority.try_transaction().unwrap();
        let (_, mut plan, _) = fixture(tx.repository_identity());
        plan.worktree = patch.worktree_path().to_str().unwrap().into();
        assert!(
            patch.inspect_validation(&tx, &plan).is_err(),
            "no inherited proof for an uninitialized module"
        );
        raw(
            patch.worktree_path(),
            &["submodule", "update", "--init", "--recursive"],
        );
        let report = patch.inspect_validation(&tx, &plan).unwrap();
        assert_eq!(
            report.source_files, 5,
            "gitignore, attributes, source, gitmodules and module source are bound"
        );
        fs::write(
            patch.worktree_path().join("deps/example/module.txt"),
            "dirty changed source",
        )
        .unwrap();
        assert!(patch.inspect_validation(&tx, &plan).is_err());
    }
}
