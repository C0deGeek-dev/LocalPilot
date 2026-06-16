//! The verification verdict — the terminal state of the §9 verification machine.

use serde::{Deserialize, Serialize};

/// The outcome of verifying one executed tool call against its contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// A postcondition (or a confirming read-back) proved the intended effect.
    Verified,
    /// The effect could not be confirmed: the contract is `Unverifiable`, or it
    /// declared no checkable postcondition. This is an honest non-success — it
    /// is never reported as a completed action.
    Unverified,
    /// A postcondition was checked and did not hold, or the call itself errored.
    Failed,
}

impl Verdict {
    /// Whether this verdict licenses a "the action succeeded" claim.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Verdict::Verified)
    }
}
