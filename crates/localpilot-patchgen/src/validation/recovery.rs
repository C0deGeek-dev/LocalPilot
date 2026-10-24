//! Static observations for explicit recovery; this module never replays an effect.

use super::{PromotionIntent, RepositoryTransaction, ValidationError};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Current source facts, not a new approval or validation capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromotionObservation {
    /// Branch currently checked out, absent when detached.
    pub current_ref: Option<String>,
    /// Exact current checkout commit.
    pub current_commit: String,
    /// Exact value of the intended target reference.
    pub target_commit: String,
    /// Whether the target working tree and index are clean.
    pub clean: bool,
}

/// Whether the exact intended source transition is observable without replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionPosition {
    /// Target remains exactly at the approved base.
    NotApplied,
    /// Target is exactly the proposed commit.
    Applied,
    /// Dirty, detached, moved or otherwise inconsistent source facts.
    Diverged,
}

impl PromotionObservation {
    /// Classify current facts conservatively; this never changes a reference.
    #[must_use]
    pub fn position(&self, goal: &PromotionIntent) -> PromotionPosition {
        if !self.clean
            || self.current_ref.as_deref() != Some(&goal.target_ref)
            || self.current_commit != self.target_commit
        {
            return PromotionPosition::Diverged;
        }
        if self.current_commit == goal.base_commit {
            PromotionPosition::NotApplied
        } else if self.current_commit == goal.proposed_commit {
            PromotionPosition::Applied
        } else {
            PromotionPosition::Diverged
        }
    }
}

impl RepositoryTransaction<'_> {
    /// Inspect exact source facts while retaining the common guard. Git helpers
    /// use the existing static policy and never execute proposed checks.
    ///
    /// # Errors
    /// Wrong repository or unavailable source/ref metadata. Missing facts never
    /// imply that an interrupted effect did not happen.
    pub fn inspect_promotion_goal(
        &self,
        root: &Path,
        goal: &PromotionIntent,
    ) -> Result<PromotionObservation, ValidationError> {
        self.check_root(root)?;
        let current_ref = match crate::git::git(root, &["symbolic-ref", "--quiet", "HEAD"]) {
            Ok(value) => Some(value.trim().to_owned()),
            Err(crate::PatchError::Git { .. }) => None,
            Err(error) => return Err(error.into()),
        };
        let revision = format!("{}^{{commit}}", goal.target_ref);
        let target_commit = crate::git::git(
            root,
            &["rev-parse", "--verify", "--end-of-options", &revision],
        )?
        .trim()
        .to_owned();
        Ok(PromotionObservation {
            current_ref,
            current_commit: crate::git::head_commit(root)?,
            target_commit,
            clean: crate::git::is_clean(root)?,
        })
    }
}
