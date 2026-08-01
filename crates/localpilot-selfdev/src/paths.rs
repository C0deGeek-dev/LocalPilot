//! Where self-dev state lives on disk.
//!
//! Under the same per-user data directory the release cache already uses, in its
//! own `selfdev` subtree — one place for a user to look, and a subtree that can
//! be deleted wholesale without disturbing installed releases.

use std::path::PathBuf;

/// Directory name for the self-dev subtree.
const SELFDEV_DIR: &str = "selfdev";

/// The default self-dev root, when the platform reports a data directory.
#[must_use]
pub fn default_root() -> Option<PathBuf> {
    localpilot_dist::Cache::default_root(crate::builder::TOOL).map(|base| base.join(SELFDEV_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_selfdev_root_sits_beside_the_release_cache_not_inside_it() {
        let Some(root) = default_root() else {
            return; // A platform with no data directory has nothing to assert.
        };
        assert!(root.ends_with(SELFDEV_DIR));
        let base = localpilot_dist::Cache::default_root(crate::builder::TOOL).expect("base");
        assert_eq!(
            root.parent(),
            Some(base.as_path()),
            "self-dev state must be a sibling subtree, deletable on its own"
        );
        assert!(
            !base.join("versions").starts_with(&root),
            "release versions must not live under the self-dev subtree"
        );
    }
}
