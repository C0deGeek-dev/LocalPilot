//! The evidence-source abstraction and a best-effort fan-out over many sources.

use async_trait::async_trait;

use crate::{Evidence, SourceAccount, SourceError};

/// One source's yield for one query: the evidence plus the retrieval account
/// explaining what was considered, admitted, rejected, skipped, or failed —
/// so "no evidence" is never ambiguous between "nothing proposed" and
/// "everything rejected" (LocalHub#33).
#[derive(Debug)]
pub struct Gathered {
    /// The evidence handed to the engine's pool.
    pub evidence: Vec<Evidence>,
    /// The retrieval account for this call.
    pub account: SourceAccount,
}

impl Gathered {
    /// Wrap plain evidence with a basic account: everything returned counts
    /// as proposed and admitted. For sources without a richer internal
    /// pipeline (and test fakes).
    #[must_use]
    pub fn from_evidence(source: impl Into<String>, evidence: Vec<Evidence>) -> Self {
        let mut account = SourceAccount::new(source);
        account.proposed = evidence.len();
        account.admitted = evidence.len();
        Self { evidence, account }
    }
}

/// A place research evidence can be gathered from.
///
/// Implementations live in the binding layer (the CLI), where filesystem,
/// network, and engine access exist; the loop depends only on this trait so it
/// stays host-neutral and unit-testable with fakes.
#[async_trait]
pub trait Source: Send + Sync {
    /// Stable label, used as the provenance source tag (e.g. `memory`, `web`).
    fn label(&self) -> &str;

    /// Gather up to `limit` snippets answering `question`, with the
    /// retrieval account for the call.
    async fn gather(&self, question: &str, limit: usize) -> Result<Gathered, SourceError>;
}

/// A best-effort fan-out over several sources. A source that errors is recorded
/// and skipped — it never aborts the run.
#[derive(Default)]
pub struct SourceSet {
    sources: Vec<Box<dyn Source>>,
}

impl SourceSet {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
        }
    }

    /// Builder-style add.
    #[must_use]
    pub fn with(mut self, source: Box<dyn Source>) -> Self {
        self.sources.push(source);
        self
    }

    /// Add a source.
    pub fn push(&mut self, source: Box<dyn Source>) {
        self.sources.push(source);
    }

    /// Whether any source is configured.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Number of configured sources.
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// The configured sources' labels, in order.
    #[must_use]
    pub fn labels(&self) -> Vec<&str> {
        self.sources.iter().map(|source| source.label()).collect()
    }

    /// Gather across every source for one question. Returns the merged
    /// evidence, any source errors, and the per-source retrieval accounts; an
    /// erroring source contributes its error and a failed account, never
    /// failing the call (best-effort, partial-result contract).
    pub async fn gather_all(
        &self,
        question: &str,
        per_source: usize,
    ) -> (Vec<Evidence>, Vec<SourceError>, Vec<SourceAccount>) {
        let mut evidence = Vec::new();
        let mut errors = Vec::new();
        let mut accounts = Vec::new();
        for source in &self.sources {
            match source.gather(question, per_source).await {
                Ok(mut gathered) => {
                    evidence.append(&mut gathered.evidence);
                    accounts.push(gathered.account);
                }
                Err(err) => {
                    let mut account = SourceAccount::new(source.label());
                    account.failed = 1;
                    accounts.push(account);
                    errors.push(err);
                }
            }
        }
        (evidence, errors, accounts)
    }
}
