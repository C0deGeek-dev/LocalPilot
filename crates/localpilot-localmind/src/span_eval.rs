//! Measuring whether span retrieval returns the right spans.
//!
//! # What this reports, and what it refuses to
//!
//! Everything shipped before this measured *coverage* — how many spans exist,
//! how fast they index, how much disk they take. None of that says whether a
//! query returns the spans it should. This does.
//!
//! It reports metrics and **locators**, never span text. That is a privacy
//! clause rather than a style choice: evaluation artefacts are corpus-derived,
//! and a report that quotes what it found is a transcript excerpt wearing the
//! name of a test result. [`QualityReport`] has no field that can hold span
//! content, so the rule is structural rather than a habit reviewers have to
//! maintain.
//!
//! # Why a query set is a fixture, not a snapshot of real sessions
//!
//! A lexical query built from a distinctive string in a real transcript *is*
//! transcript content, and committing it puts that content in a repository. So
//! the committed query set runs against a **synthetic corpus** authored for the
//! purpose, which also makes it deterministic and runnable in CI. Measurements
//! over the real corpus are run locally and only their *numbers* are recorded.
//!
//! # Query classes
//!
//! The classes fail differently, and an aggregate number hides that:
//!
//! - **Lexical** — an identifier, filename or error string. Keyword search
//!   should already win these; if it does not, nothing else will help.
//! - **Paraphrase** — asks for a concept in words the span does not use. This is
//!   the case a dense retriever exists for, and the case a keyword index is
//!   expected to lose.
//! - **Cross-session** — the answer needs spans from more than one session. This
//!   is the capability that motivated the work.
//! - **Redaction-affected** — the relevant span had its most distinctive tokens
//!   redacted. Indexing the redacted form is the right choice and it has a
//!   price; a plan that never measures the price cannot later tell a redaction
//!   artefact from a design failure.
//! - **Negative** — nothing relevant exists, and returning nothing is correct.
//!   Over-retrieval is invisible to recall, so without this class a system that
//!   answers everything scores perfectly.

use std::collections::{BTreeMap, BTreeSet};

/// How a query is expected to fail if it fails.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum QueryClass {
    /// Keyword search should win outright.
    Lexical,
    /// The words asked with are not the words stored.
    Paraphrase,
    /// The answer needs more than one session.
    CrossSession,
    /// Redaction removed the tokens that would have matched.
    RedactionAffected,
    /// Nothing relevant exists; retrieving nothing is the right answer.
    Negative,
    /// Looks relevant and is not — a query that attracts plausible distractors
    /// while nothing actually answers it.
    ///
    /// Distinct from [`QueryClass::Negative`], and the distinction is the point:
    /// a negative query attracts nothing, a near-miss attracts candidates and
    /// none of them are right. A retriever can pass one and fail the other.
    NearMiss,
}

impl QueryClass {
    /// A short, stable name for reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            QueryClass::Lexical => "lexical",
            QueryClass::Paraphrase => "paraphrase",
            QueryClass::CrossSession => "cross-session",
            QueryClass::RedactionAffected => "redaction-affected",
            QueryClass::Negative => "negative",
            QueryClass::NearMiss => "near-miss",
        }
    }
}

/// One query and the locators a perfect system would return for it.
#[derive(Clone, Debug)]
pub struct EvalQuery {
    /// Stable identifier, so a result can be traced back to its query.
    pub id: String,
    /// What is asked.
    pub text: String,
    /// Which failure mode this query probes.
    pub class: QueryClass,
    /// The locators that should be returned. Empty for [`QueryClass::Negative`],
    /// where the correct answer is to return nothing.
    pub relevant: Vec<String>,
    /// Every id judged for this query — relevant and irrelevant alike.
    ///
    /// `None` means **the pool is complete**: every id the retriever can return
    /// was judged, so nothing can come back unjudged. That holds for a synthetic
    /// corpus, where the fixture *is* the whole world.
    ///
    /// `Some(ids)` means the pool was drawn from a larger corpus, so a retriever
    /// can return something nobody judged. Such an id is **`UNJUDGED`** — never
    /// counted as irrelevant, because scoring it that way understates precision:
    /// a genuinely relevant memory that never reached the pool would be counted
    /// as a wrong answer.
    pub judged: Option<Vec<String>>,
}

/// What one query scored.
#[derive(Clone, Debug)]
pub struct QueryOutcome {
    /// The query's id.
    pub id: String,
    /// Its class.
    pub class: QueryClass,
    /// How many relevant locators exist.
    pub relevant: usize,
    /// How many of them were returned within the cut.
    pub hit: usize,
    /// How many results were returned.
    pub returned: usize,
    /// Rank of the first relevant result, 1-based. `None` if none was returned.
    pub first_relevant_rank: Option<usize>,
    /// Returned ids that nobody judged. Always zero when the pool is complete.
    pub unjudged: usize,
}

impl QueryOutcome {
    /// Fraction of the relevant locators that were returned.
    ///
    /// A negative query has no relevant locators, so recall is undefined rather
    /// than zero — reporting zero would drag the average down for queries that
    /// behaved perfectly.
    #[must_use]
    pub fn recall(&self) -> Option<f64> {
        if self.relevant == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.hit as f64 / self.relevant as f64)
    }

    /// Fraction of returned results that were relevant.
    ///
    /// Undefined when nothing was returned. For a negative query, returning
    /// nothing is success and is scored by [`QueryOutcome::negative_correct`]
    /// instead.
    #[must_use]
    pub fn precision(&self) -> Option<f64> {
        if self.returned == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.hit as f64 / self.returned as f64)
    }

    /// Whether a negative query correctly returned nothing.
    ///
    /// The one metric that catches over-retrieval, which recall cannot see: a
    /// system that returns everything has perfect recall.
    #[must_use]
    pub fn negative_correct(&self) -> Option<bool> {
        (self.relevant == 0).then_some(self.returned == 0)
    }

    /// Precision counting every unjudged result as irrelevant — the pessimistic
    /// end of the range.
    ///
    /// Undefined when nothing was returned.
    #[must_use]
    pub fn precision_lower(&self) -> Option<f64> {
        self.precision()
    }

    /// Precision counting every unjudged result as relevant — the optimistic end.
    ///
    /// The two bounds are reported together, never averaged. When they are far
    /// apart the honest statement is that the measurement is not precise enough
    /// yet, not that the truth is in the middle.
    #[must_use]
    pub fn precision_upper(&self) -> Option<f64> {
        if self.returned == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some((self.hit + self.unjudged) as f64 / self.returned as f64)
    }

    /// The fraction of returned ids that were judged at all.
    ///
    /// Reported with every precision figure, because a precision at 40% coverage
    /// and one at 95% are not comparable and a report that omits this invites
    /// exactly that comparison.
    #[must_use]
    pub fn coverage(&self) -> Option<f64> {
        if self.returned == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some((self.returned - self.unjudged) as f64 / self.returned as f64)
    }

    /// Reciprocal of the first relevant rank — 1.0 when the top result is
    /// relevant, 0.0 when none is.
    #[must_use]
    pub fn reciprocal_rank(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        self.first_relevant_rank
            .map_or(0.0, |rank| 1.0 / rank as f64)
    }
}

/// Aggregate numbers for one class of query.
#[derive(Clone, Debug, Default)]
pub struct ClassSummary {
    /// Queries in this class.
    pub queries: usize,
    /// Mean recall over queries where recall is defined.
    pub recall: Option<f64>,
    /// Mean precision over queries that returned something.
    pub precision: Option<f64>,
    /// Mean reciprocal rank.
    pub mrr: f64,
    /// For the negative class: how many correctly returned nothing.
    pub negatives_correct: Option<usize>,
    /// Mean precision counting unjudged results as irrelevant.
    pub precision_lower: Option<f64>,
    /// Mean precision counting unjudged results as relevant.
    pub precision_upper: Option<f64>,
    /// Mean fraction of returned ids that were judged.
    pub coverage: Option<f64>,
    /// Total returned ids nobody judged, across the class.
    pub unjudged: usize,
}

/// The whole measurement.
///
/// Deliberately holds no span text — see the module docs. A report is safe to
/// commit because there is nowhere in it for corpus content to sit.
#[derive(Clone, Debug)]
pub struct QualityReport {
    /// The rank cut every metric is taken at.
    pub cut: usize,
    /// Per-query outcomes, in query order.
    pub outcomes: Vec<QueryOutcome>,
    /// Per-class aggregates.
    pub by_class: BTreeMap<&'static str, ClassSummary>,
}

impl QualityReport {
    /// Render the report as plain text: metrics and counts, never content.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!("Span retrieval quality (top {}):\n\n", self.cut);
        out.push_str("class                queries  recall  precision   mrr\n");
        for (name, summary) in &self.by_class {
            let recall = summary
                .recall
                .map_or_else(|| "   n/a".to_string(), |value| format!("{value:6.2}"));
            let precision = summary
                .precision
                .map_or_else(|| "      n/a".to_string(), |value| format!("{value:9.2}"));
            out.push_str(&format!(
                "{:<20} {:>7}  {recall}  {precision}  {:>4.2}\n",
                name, summary.queries, summary.mrr
            ));
            if let Some(correct) = summary.negatives_correct {
                out.push_str(&format!(
                    "{:<20} {correct} of {} correctly returned nothing\n",
                    "", summary.queries
                ));
            }
        }
        out.push_str("\nper query:\n");
        for outcome in &self.outcomes {
            let rank = outcome
                .first_relevant_rank
                .map_or_else(|| "-".to_string(), |rank| rank.to_string());
            out.push_str(&format!(
                "  {:<14} {:<18} relevant {} hit {} returned {} first-rank {rank}\n",
                outcome.id,
                outcome.class.name(),
                outcome.relevant,
                outcome.hit,
                outcome.returned
            ));
        }
        out
    }
}

/// The synthetic sessions the committed measurement runs against.
///
/// Invented engineering conversation. Nothing here is drawn from a real
/// transcript, which is what makes it committable: a query built from a
/// distinctive string in a real session *is* session content.
#[must_use]
pub fn synthetic_corpus() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    vec![
        (
            "session-alpha",
            vec![
                (
                    "user",
                    "the vector_scan_kind helper returns nothing for docs",
                ),
                (
                    "assistant",
                    "it filters by subject kind before the cosine gate, so a shared top-k \
                     cannot starve one kind",
                ),
                (
                    "user",
                    "how do we stop one kind of row crowding out another",
                ),
            ],
        ),
        (
            "session-beta",
            vec![
                (
                    "user",
                    "build fails with error[E0004]: non-exhaustive patterns",
                ),
                (
                    "assistant",
                    "a new enum variant needs a match arm in every consumer",
                ),
            ],
        ),
        (
            "session-gamma",
            vec![
                (
                    "user",
                    "the release needs every repository at the same version before tagging",
                ),
                (
                    "assistant",
                    "check the submodule pointer first, then cut, then verify the published \
                     artefacts exist",
                ),
            ],
        ),
        (
            "session-delta",
            vec![
                (
                    "user",
                    "why did the tag succeed but the command report failure",
                ),
                (
                    "assistant",
                    "the publish step exited non-zero after the artefact was already created, \
                     so the failure is reported and the work is done",
                ),
            ],
        ),
        (
            "session-epsilon",
            vec![
                (
                    "user",
                    "connect using the credential [REDACTED] and retry the request",
                ),
                (
                    "assistant",
                    "the value is hidden, so search it by what surrounds it instead",
                ),
            ],
        ),
    ]
}

/// The locator for a session's Nth span, so a query's relevant set is written
/// against positions rather than against text.
#[must_use]
pub fn synthetic_locator(session: &str, ordinal: usize) -> String {
    format!("span:{session}:1:{ordinal}")
}

/// The frozen query set.
///
/// Written against [`synthetic_corpus`] **before** anything was scored. A query
/// set adjusted after seeing results measures nothing, so this function is the
/// artefact that must not move in response to a number.
#[must_use]
pub fn query_set() -> Vec<EvalQuery> {
    vec![
        // Lexical: an identifier that appears in exactly one span. Keyword
        // search should win outright; if it does not, nothing else will.
        EvalQuery {
            id: "L1".into(),
            text: "vector_scan_kind".into(),
            class: QueryClass::Lexical,
            judged: None,
            relevant: vec![synthetic_locator("session-alpha", 0)],
        },
        // Lexical: an error string, the other shape a keyword index is for.
        EvalQuery {
            id: "L2".into(),
            text: "error E0004 non-exhaustive patterns".into(),
            class: QueryClass::Lexical,
            judged: None,
            relevant: vec![synthetic_locator("session-beta", 0)],
        },
        // Paraphrase: the answer uses "subject kind" and "starve"; the query
        // uses neither. This is the case a dense retriever exists for.
        EvalQuery {
            id: "P1".into(),
            text: "prevent one category of result dominating the others".into(),
            class: QueryClass::Paraphrase,
            judged: None,
            relevant: vec![synthetic_locator("session-alpha", 1)],
        },
        // Paraphrase: "succeeded but reported failure" against wording that says
        // "exited non-zero after the artefact was already created".
        EvalQuery {
            id: "P2".into(),
            text: "the operation actually worked despite reporting an error".into(),
            class: QueryClass::Paraphrase,
            judged: None,
            relevant: vec![synthetic_locator("session-delta", 1)],
        },
        // Cross-session: version parity is discussed in gamma, and the
        // false-failure that bites during a release in delta. Answering well
        // needs both — the capability that motivated the work.
        EvalQuery {
            id: "X1".into(),
            text: "release version tagging failure".into(),
            class: QueryClass::CrossSession,
            judged: None,
            relevant: vec![
                synthetic_locator("session-gamma", 0),
                synthetic_locator("session-gamma", 1),
                synthetic_locator("session-delta", 0),
            ],
        },
        // Redaction-affected: the distinctive token is gone from the span, so
        // only its surrounding words can match. Indexing the redacted form is
        // correct and this is its price.
        EvalQuery {
            id: "R1".into(),
            text: "connect using the credential and retry".into(),
            class: QueryClass::RedactionAffected,
            judged: None,
            relevant: vec![synthetic_locator("session-epsilon", 0)],
        },
        // Negative: nothing in this corpus is about any of these. Returning
        // nothing is the correct answer, and this class is the only thing that
        // catches over-retrieval — recall cannot see it.
        EvalQuery {
            id: "N1".into(),
            text: "kubernetes ingress controller".into(),
            class: QueryClass::Negative,
            judged: None,
            relevant: vec![],
        },
        EvalQuery {
            id: "N2".into(),
            text: "photosynthesis chlorophyll".into(),
            class: QueryClass::Negative,
            judged: None,
            relevant: vec![],
        },
    ]
}

/// Precomputed vectors for the synthetic corpus and the frozen query set.
///
/// **Evaluation scaffolding, not a shipped retriever.** Dense session retrieval
/// is out of scope until the numbers justify it; this exists so those numbers
/// can be produced offline and deterministically. A comparison that needs a live
/// model server is a comparison that silently stops happening.
///
/// Keys are span locators and `query:<id>`, so a fixture entry is addressed by
/// exactly the locator the index emits — deriving the key any other way would
/// let the fixture drift from what it describes.
#[derive(Clone, Debug)]
pub struct DenseFixture {
    model: String,
    vectors: BTreeMap<String, Vec<f32>>,
}

impl DenseFixture {
    /// Parse a fixture produced by the `span_dense_fixture` example.
    ///
    /// # Errors
    /// Returns a message when the JSON is not a fixture.
    pub fn parse(json: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(json).map_err(|error| error.to_string())?;
        let model = value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .ok_or("fixture has no model")?
            .to_string();
        let table = value
            .get("vectors")
            .and_then(serde_json::Value::as_object)
            .ok_or("fixture has no vectors")?;
        let mut vectors = BTreeMap::new();
        for (key, raw) in table {
            let vector: Vec<f32> = raw
                .as_array()
                .ok_or("a vector is not an array")?
                .iter()
                .map(|value| value.as_f64().unwrap_or_default() as f32)
                .collect();
            if vector.is_empty() {
                return Err(format!("empty vector for {key}"));
            }
            vectors.insert(key.clone(), vector);
        }
        Ok(Self { model, vectors })
    }

    /// Which model produced the vectors. Recorded so a comparison states what it
    /// compared, rather than leaving "dense" to mean whatever was installed.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The best cosine any span achieves for a query.
    ///
    /// Reported so the comparison can say whether a similarity floor is *viable*
    /// without choosing one. Picking a threshold after seeing which queries it
    /// helps is tuning; measuring whether positives and negatives separate at all
    /// is evidence.
    #[must_use]
    pub fn top_similarity(&self, query_id: &str) -> Option<f32> {
        let query = self.vectors.get(&format!("query:{query_id}"))?;
        self.vectors
            .iter()
            .filter(|(key, _)| key.starts_with("span:"))
            .map(|(_, vector)| cosine(query, vector))
            .fold(None, |best: Option<f32>, value| {
                Some(best.map_or(value, |best| best.max(value)))
            })
    }

    /// The best cosine any span achieves for a caller-supplied vector.
    ///
    /// Exists so a live endpoint's output can be scored against the same corpus
    /// as the cached vectors, which is what makes "the offline baseline is the
    /// live baseline" a checkable claim rather than an assumption.
    #[must_use]
    pub fn top_similarity_for(&self, query: &[f32]) -> Option<f32> {
        self.vectors
            .iter()
            .filter(|(key, _)| key.starts_with("span:"))
            .map(|(_, vector)| cosine(query, vector))
            .fold(None, |best: Option<f32>, value| {
                Some(best.map_or(value, |best| best.max(value)))
            })
    }

    /// Locators ranked by cosine against a query's vector, best first.
    ///
    /// Only spans are ranked; `query:` keys are the query side of the fixture.
    /// A query with no vector returns nothing — an absent fixture entry must not
    /// silently score as a miss that looks like a retrieval failure.
    #[must_use]
    pub fn rank(&self, query_id: &str, cut: usize) -> Option<Vec<String>> {
        let query = self.vectors.get(&format!("query:{query_id}"))?;
        let mut scored: Vec<(String, f32)> = self
            .vectors
            .iter()
            .filter(|(key, _)| key.starts_with("span:"))
            .map(|(key, vector)| (key.clone(), cosine(query, vector)))
            .collect();
        // Ties break by locator so the ranking is deterministic — two spans with
        // identical similarity must not swap between runs.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        Some(scored.into_iter().take(cut).map(|(key, _)| key).collect())
    }
}

/// Cosine similarity. Zero when either side has no magnitude, which is the
/// honest answer for a vector that carries no direction.
fn cosine(left: &[f32], right: &[f32]) -> f32 {
    let mut dot = 0.0_f32;
    let mut left_norm = 0.0_f32;
    let mut right_norm = 0.0_f32;
    for (a, b) in left.iter().zip(right.iter()) {
        dot += a * b;
        left_norm += a * a;
        right_norm += b * b;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return 0.0;
    }
    dot / (left_norm.sqrt() * right_norm.sqrt())
}

/// A frozen judgment set read from a `trec_eval` qrels file.
///
/// Format is one judgment per line: `<query-id> 0 <document-id> <relevance>`.
/// The `0` column is trec's unused iteration field, kept so the file is readable
/// by a standard tool nobody here has to write.
///
/// The set is addressed by the **SHA-256 of the file**, and a caller is expected
/// to check it before quoting any number taken against it. A judgment set that
/// changed under a measurement invalidates the measurement, and the hash is the
/// only thing that makes that detectable rather than assumed.
#[derive(Clone, Debug)]
pub struct JudgmentSet {
    digest: String,
    queries: Vec<EvalQuery>,
}

impl JudgmentSet {
    /// Parse qrels text, taking each query's class and prompt from `metadata`
    /// keyed by query id.
    ///
    /// # Errors
    /// Returns a message when a line is not four whitespace-separated fields or
    /// the relevance column is not an integer.
    pub fn parse(
        qrels: &str,
        metadata: &BTreeMap<String, (QueryClass, String)>,
    ) -> Result<Self, String> {
        let mut relevant: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut judged: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (number, line) in qrels.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let [query, _iteration, document, grade] = fields.as_slice() else {
                return Err(format!("line {}: expected four fields", number + 1));
            };
            let grade: i32 = grade
                .parse()
                .map_err(|_| format!("line {}: relevance is not an integer", number + 1))?;
            judged
                .entry((*query).to_string())
                .or_default()
                .push((*document).to_string());
            if grade > 0 {
                relevant
                    .entry((*query).to_string())
                    .or_default()
                    .push((*document).to_string());
            }
        }
        let mut queries = Vec::new();
        for (id, judged_ids) in judged {
            let (class, text) = metadata
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("{id}: no class/prompt metadata"))?;
            queries.push(EvalQuery {
                relevant: relevant.get(&id).cloned().unwrap_or_default(),
                judged: Some(judged_ids),
                id,
                text,
                class,
            });
        }
        Ok(Self {
            digest: sha256_hex(qrels.as_bytes()),
            queries,
        })
    }

    /// The SHA-256 of the qrels text this set was parsed from.
    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The queries, in id order.
    #[must_use]
    pub fn queries(&self) -> &[EvalQuery] {
        &self.queries
    }
}

/// SHA-256, so a judgment set can be addressed by content without a dependency.
///
/// Small and self-contained on purpose: the alternative is pulling a hashing
/// crate into a library that needs exactly one digest, in one place, for one
/// integrity check.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let mut message = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0_u32; 64];
        for (index, word) in w.iter_mut().enumerate().take(16) {
            let start = index * 4;
            *word = u32::from_be_bytes([
                chunk[start],
                chunk[start + 1],
                chunk[start + 2],
                chunk[start + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *slot = slot.wrapping_add(value);
        }
    }
    use std::fmt::Write as _;
    h.iter().fold(String::with_capacity(64), |mut out, word| {
        let _ = write!(out, "{word:08x}");
        out
    })
}

/// Score one query's results against its known-relevant locators.
///
/// `returned` is the ordered list of locators the retrieval path produced.
#[must_use]
pub fn score_query(query: &EvalQuery, returned: &[String], cut: usize) -> QueryOutcome {
    let relevant: BTreeSet<&str> = query.relevant.iter().map(String::as_str).collect();
    let cut_results: Vec<&String> = returned.iter().take(cut).collect();
    let mut hit = 0;
    let mut first_relevant_rank = None;
    for (index, locator) in cut_results.iter().enumerate() {
        if relevant.contains(locator.as_str()) {
            hit += 1;
            if first_relevant_rank.is_none() {
                first_relevant_rank = Some(index + 1);
            }
        }
    }
    // An id nobody judged is UNJUDGED, not irrelevant. With a complete pool
    // (`judged: None`) this is always zero by construction.
    let unjudged = match &query.judged {
        None => 0,
        Some(judged) => {
            let judged: BTreeSet<&str> = judged.iter().map(String::as_str).collect();
            cut_results
                .iter()
                .filter(|id| !judged.contains(id.as_str()))
                .count()
        }
    };
    QueryOutcome {
        id: query.id.clone(),
        class: query.class,
        relevant: relevant.len(),
        hit,
        returned: cut_results.len(),
        first_relevant_rank,
        unjudged,
    }
}

/// Aggregate scored queries into a report.
#[must_use]
pub fn summarise(outcomes: Vec<QueryOutcome>, cut: usize) -> QualityReport {
    let mut by_class: BTreeMap<&'static str, ClassSummary> = BTreeMap::new();
    for outcome in &outcomes {
        let entry = by_class.entry(outcome.class.name()).or_default();
        entry.queries += 1;
    }
    for (name, summary) in &mut by_class {
        let members: Vec<&QueryOutcome> = outcomes
            .iter()
            .filter(|outcome| outcome.class.name() == *name)
            .collect();
        summary.recall = mean(members.iter().filter_map(|outcome| outcome.recall()));
        summary.precision = mean(members.iter().filter_map(|outcome| outcome.precision()));
        summary.precision_lower = mean(
            members
                .iter()
                .filter_map(|outcome| outcome.precision_lower()),
        );
        summary.precision_upper = mean(
            members
                .iter()
                .filter_map(|outcome| outcome.precision_upper()),
        );
        summary.coverage = mean(members.iter().filter_map(|outcome| outcome.coverage()));
        summary.unjudged = members.iter().map(|outcome| outcome.unjudged).sum();
        summary.mrr =
            mean(members.iter().map(|outcome| outcome.reciprocal_rank())).unwrap_or_default();
        let negatives: Vec<bool> = members
            .iter()
            .filter_map(|outcome| outcome.negative_correct())
            .collect();
        if !negatives.is_empty() {
            summary.negatives_correct = Some(negatives.iter().filter(|value| **value).count());
        }
    }
    QualityReport {
        cut,
        outcomes,
        by_class,
    }
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0_usize;
    for value in values {
        total += value;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    #[allow(clippy::cast_precision_loss)]
    Some(total / count as f64)
}
