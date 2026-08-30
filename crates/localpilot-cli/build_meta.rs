//! Resolving the build metadata this binary embeds — and, just as importantly,
//! deciding whether the build script has to watch the repository to stay
//! truthful.
//!
//! Included by both `build.rs` (which runs it) and the crate's tests (which
//! assert it), so the policy has exactly one definition. A build script cannot
//! be unit-tested where it lives; a file included from both places can be.
//!
//! **Why the watching question matters.** A build script that watches `.git` is
//! re-run on every commit, which is correct for an ordinary source build — the
//! embedded `git describe` version would otherwise go stale the moment you
//! commit. But a caller that already *knows* the identity (the self-dev build
//! wrapper, or a release pipeline) passes it in, and then watching `.git` only
//! buys a rebuild nobody asked for. So the rule is: watch the repository if and
//! only if a value was read from it.

/// What a build embeds, and what it must watch to keep that honest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildMeta {
    /// Human-facing version string.
    pub(crate) version: String,
    /// Commit hash, or `unknown` when it could not be determined.
    pub(crate) git_hash: String,
    /// Fingerprint of the source tree, when the caller supplied one.
    pub(crate) fingerprint: Option<String>,
    /// Whether the build script must re-run when the repository moves.
    pub(crate) watch_git: bool,
}

/// The hash reported when no commit could be resolved at all.
pub(crate) const UNKNOWN_HASH: &str = "unknown";

/// Resolve the metadata to embed.
///
/// `describe` and `head` are only consulted when the environment did not already
/// answer, so a caller that supplies both never pays for a `git` invocation.
pub(crate) fn resolve(
    env_version: Option<String>,
    env_hash: Option<String>,
    env_fingerprint: Option<String>,
    package_version: &str,
    describe: impl FnOnce() -> Option<String>,
    head: impl FnOnce() -> Option<String>,
) -> BuildMeta {
    let mut read_from_repo = false;

    let version = match non_empty(env_version) {
        Some(version) => version,
        None => match describe() {
            Some(described) => {
                read_from_repo = true;
                described
            }
            None => package_version.to_string(),
        },
    };

    let git_hash = match non_empty(env_hash) {
        Some(hash) => hash,
        None => match head() {
            Some(hash) => {
                read_from_repo = true;
                hash
            }
            None => UNKNOWN_HASH.to_string(),
        },
    };

    BuildMeta {
        version,
        git_hash,
        fingerprint: non_empty(env_fingerprint),
        watch_git: read_from_repo,
    }
}

/// An environment value counts only when it carries something.
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
