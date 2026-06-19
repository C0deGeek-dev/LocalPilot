//! The patch proposal: the edits an approved finding suggests, scope-bound to the
//! files that finding names.

use serde::{Deserialize, Serialize};

use crate::error::PatchError;

/// One proposed whole-file edit: create or replace `path` with `new_content`.
/// Whole-file content keeps the proposal self-describing and the apply trivially
/// containable (write one file), rather than carrying a fragile positional diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposedEdit {
    /// Project-relative path (forward-slashed) to write.
    pub path: String,
    /// The full new file content.
    pub new_content: String,
}

impl ProposedEdit {
    /// A new edit.
    #[must_use]
    pub fn new(path: impl Into<String>, new_content: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            new_content: new_content.into(),
        }
    }
}

/// A scope-bound set of edits addressing one finding. `allowed_paths` are the only
/// files the proposal may touch (the files the finding named); any edit outside
/// that set is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchProposal {
    /// The finding evidence this proposal addresses (for the provenance/diff).
    pub finding_evidence: String,
    /// The files the finding named — the scope bound.
    pub allowed_paths: Vec<String>,
    /// The proposed edits.
    pub edits: Vec<ProposedEdit>,
}

impl PatchProposal {
    /// A new proposal.
    #[must_use]
    pub fn new(
        finding_evidence: impl Into<String>,
        allowed_paths: Vec<String>,
        edits: Vec<ProposedEdit>,
    ) -> Self {
        Self {
            finding_evidence: finding_evidence.into(),
            allowed_paths,
            edits,
        }
    }

    /// Validate the scope contract: at least one edit, and every edit targets a
    /// file in `allowed_paths` (path comparison is forward-slash-normalised).
    /// An out-of-scope edit rejects the whole proposal.
    ///
    /// # Errors
    /// [`PatchError::EmptyProposal`] when there are no edits;
    /// [`PatchError::OutOfScope`] when an edit targets an unnamed file.
    pub fn validate_scope(&self) -> Result<(), PatchError> {
        if self.edits.is_empty() {
            return Err(PatchError::EmptyProposal);
        }
        let allowed: Vec<String> = self.allowed_paths.iter().map(|p| normalize(p)).collect();
        for edit in &self.edits {
            if !allowed.contains(&normalize(&edit.path)) {
                return Err(PatchError::OutOfScope(edit.path.clone()));
            }
        }
        Ok(())
    }
}

/// Forward-slash normalisation for path comparison, so `a\b` and `a/b` and
/// `./a/b` compare equal.
pub(crate) fn normalize(path: &str) -> String {
    let slashed = path.replace('\\', "/");
    slashed.strip_prefix("./").unwrap_or(&slashed).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(allowed: &[&str], edits: &[(&str, &str)]) -> PatchProposal {
        PatchProposal::new(
            "a finding",
            allowed.iter().map(|s| s.to_string()).collect(),
            edits
                .iter()
                .map(|(p, c)| ProposedEdit::new(*p, *c))
                .collect(),
        )
    }

    #[test]
    fn in_scope_proposal_validates() {
        assert!(proposal(&["src/a.rs"], &[("src/a.rs", "x")])
            .validate_scope()
            .is_ok());
        // Path spelling differences normalise.
        assert!(proposal(&["src/a.rs"], &[("./src/a.rs", "x")])
            .validate_scope()
            .is_ok());
    }

    #[test]
    fn out_of_scope_edit_is_rejected() {
        let err = proposal(&["src/a.rs"], &[("src/b.rs", "x")])
            .validate_scope()
            .unwrap_err();
        assert!(matches!(err, PatchError::OutOfScope(_)));
    }

    #[test]
    fn empty_proposal_is_rejected() {
        assert!(matches!(
            proposal(&["src/a.rs"], &[]).validate_scope().unwrap_err(),
            PatchError::EmptyProposal
        ));
    }
}
