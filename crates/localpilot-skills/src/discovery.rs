//! Review-only skill *discovery*: finding skills relevant to a query across the
//! effective catalog and registered sources, ranking them, validating a candidate
//! repository read-only, and saving a de-duplicated review proposal (LocalHub#41).
//!
//! Discovery never registers a source, installs a skill, or executes any fetched
//! content — it only *reads* and *recommends*. Every mutation stays with the
//! management surface ([`crate::manager::SkillsManager`]); discovery produces
//! proposals a human reviews and acts on there. The model may classify and
//! recommend through the [`SkillClassifier`] seam, but it can never invoke a
//! mutation from here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::{read_catalog, CatalogPackage};
use crate::error::SkillError;
use crate::fetch::{ensure_snapshot_within_bounds, RepoFetcher};
use crate::source::normalize_url;

/// Where a matched skill stands relative to the user's current setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MatchState {
    /// Already present in the effective skill catalog (project + global).
    Installed,
    /// Present in a registered source's cached catalog but not installed.
    Available,
    /// Found in a new, unregistered repository during web discovery.
    Discovered,
}

impl MatchState {
    /// A short label for reports and the review surface.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MatchState::Installed => "installed",
            MatchState::Available => "available",
            MatchState::Discovered => "discovered",
        }
    }
}

/// One skill surfaced by discovery, with the evidence needed to classify, rank,
/// and (for an unregistered repository) propose it for review. A skill that is
/// already installed carries no repository fields; an available or discovered one
/// records where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSkill {
    pub name: String,
    pub description: String,
    pub state: MatchState,
    /// The normalized repository URL an available/discovered skill came from.
    pub repo_url: Option<String>,
    /// The registered source id, for an available skill.
    pub source_id: Option<String>,
    /// The snapshot commit the skill was seen at.
    pub commit: Option<String>,
    /// The catalog root the package lives under (e.g. `.localpilot/skills`).
    pub catalog_root: Option<String>,
    /// The package path relative to the repository root.
    pub source_path: Option<String>,
    /// Whether an *installed* skill is model-discoverable (not user-only). Only an
    /// installed, discoverable skill may be auto-loaded into a research run under
    /// `[skills].autonomous_discovery`; everything else stays report-only.
    pub discoverable: bool,
}

impl DiscoveredSkill {
    /// Build an available (registered, not installed) match from a cached catalog
    /// package and its source.
    #[must_use]
    pub fn available(
        package: &CatalogPackage,
        source_id: &str,
        repo_url: &str,
        commit: &str,
        catalog_root: &str,
    ) -> Self {
        Self {
            name: package.name.clone(),
            description: package.description.clone(),
            state: MatchState::Available,
            repo_url: Some(repo_url.to_string()),
            source_id: Some(source_id.to_string()),
            commit: Some(commit.to_string()),
            catalog_root: Some(catalog_root.to_string()),
            source_path: Some(package.source_path.clone()),
            discoverable: false,
        }
    }

    /// Build a discovered (unregistered repository) match from a validated catalog
    /// package and its repository.
    #[must_use]
    pub fn discovered(package: &CatalogPackage, repo: &ValidatedRepo) -> Self {
        Self {
            name: package.name.clone(),
            description: package.description.clone(),
            state: MatchState::Discovered,
            repo_url: Some(repo.url.clone()),
            source_id: None,
            commit: Some(repo.commit.clone()),
            catalog_root: Some(repo.catalog_root.clone()),
            source_path: Some(package.source_path.clone()),
            discoverable: false,
        }
    }
}

// --- ranking ----------------------------------------------------------------

/// An optional model classifier that may pick a clearer primary recommendation
/// and rationale from the ranked candidates. It classifies only — it can never
/// invoke a repository or installation mutation. When absent, the deterministic
/// baseline stands alone, which keeps discovery reproducible and tests hermetic.
pub trait SkillClassifier {
    /// Given the query and the deterministically ranked candidates, optionally
    /// return a primary recommendation. Returning `None` defers to the baseline.
    fn recommend(&self, query: &str, candidates: &[RankedSkill]) -> Option<Recommendation>;
}

/// How a primary recommendation was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecommendationSource {
    /// The deterministic term-matching baseline.
    Deterministic,
    /// The optional model classifier.
    Model,
}

/// The single primary skill recommended for a query, with a confidence in
/// `0.0..=1.0` and a human-readable reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Recommendation {
    pub name: String,
    pub confidence: f32,
    pub reason: String,
    pub source: RecommendationSource,
}

/// One candidate with its deterministic relevance score and the reason behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct RankedSkill {
    pub skill: DiscoveredSkill,
    /// Deterministic term-match score in `0.0..=1.0`.
    pub score: f32,
    pub reason: String,
}

/// A ranked candidate list plus the chosen primary recommendation.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    /// Candidates sorted by descending score, ties broken by name for stability.
    pub matches: Vec<RankedSkill>,
    /// The primary recommendation, or `None` when nothing matched the query.
    pub recommendation: Option<Recommendation>,
}

/// Rank `candidates` against `query` with a deterministic term-matching baseline,
/// then let an optional [`SkillClassifier`] refine the primary recommendation.
///
/// The baseline is stable and independent of input order: candidates sort by
/// descending score with name as the tie-breaker, and the primary pick is the
/// top-scoring candidate. A classifier may override only the primary
/// recommendation (its name must be one of the candidates); it never reorders or
/// rescores, so the visible ranking stays reproducible.
#[must_use]
pub fn rank(
    query: &str,
    candidates: Vec<DiscoveredSkill>,
    classifier: Option<&dyn SkillClassifier>,
) -> Ranked {
    let query_terms = terms(query);
    let mut matches: Vec<RankedSkill> = candidates
        .into_iter()
        .map(|skill| {
            let (score, reason) = score_candidate(&query_terms, &skill);
            RankedSkill {
                skill,
                score,
                reason,
            }
        })
        .collect();
    // Descending score, then name ascending — a total, deterministic order.
    matches.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.skill.name.cmp(&b.skill.name))
    });

    let baseline = matches
        .iter()
        .find(|m| m.score > 0.0)
        .map(|m| Recommendation {
            name: m.skill.name.clone(),
            confidence: m.score,
            reason: m.reason.clone(),
            source: RecommendationSource::Deterministic,
        });

    // The classifier may refine the primary pick, but only to a real candidate.
    let recommendation = classifier
        .and_then(|c| c.recommend(query, &matches))
        .filter(|r| matches.iter().any(|m| m.skill.name == r.name))
        .or(baseline);

    Ranked {
        matches,
        recommendation,
    }
}

/// Score one candidate by query-term overlap over its name and description, with
/// a small bonus when a query term hits the name. The score is clamped to
/// `0.0..=1.0`; the reason names the matched terms.
fn score_candidate(query_terms: &BTreeSet<String>, skill: &DiscoveredSkill) -> (f32, String) {
    if query_terms.is_empty() {
        return (0.0, "no query terms".to_string());
    }
    let name_terms = terms(&skill.name);
    let text_terms: BTreeSet<String> = terms(&format!("{} {}", skill.name, skill.description));
    let matched: Vec<&String> = query_terms
        .iter()
        .filter(|t| text_terms.contains(*t))
        .collect();
    if matched.is_empty() {
        return (0.0, "no matching terms".to_string());
    }
    let base = matched.len() as f32 / query_terms.len() as f32;
    let name_hit = query_terms.iter().any(|t| name_terms.contains(t));
    let bonus = if name_hit { 0.25 } else { 0.0 };
    let score = (base + bonus).min(1.0);
    let matched_terms: Vec<&str> = matched.iter().map(|s| s.as_str()).collect();
    (
        score,
        format!("matched terms: {}", matched_terms.join(", ")),
    )
}

/// Tokenize into a set of lowercased alphanumeric terms of length >= 2, so short
/// stopword-like fragments do not dominate the overlap.
fn terms(text: &str) -> BTreeSet<String> {
    text.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(str::to_string)
        .collect()
}

// --- read-only repository validation ----------------------------------------

/// A validated candidate repository: its normalized URL, resolved commit, the one
/// selected catalog root, and the packages it offers. Produced by fetching a
/// snapshot and validating its catalog **without** registering or installing
/// anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedRepo {
    pub url: String,
    pub commit: String,
    pub catalog_root: String,
    pub packages: Vec<CatalogPackage>,
}

/// Fetch `url` into `staging` and validate its catalog read-only, returning the
/// [`ValidatedRepo`] a proposal is built from. Nothing in the fetched tree is
/// executed and nothing is registered or installed.
///
/// # Errors
/// Rejects a non-HTTPS/credential-bearing URL ([`SkillError::Rejected`]), reports
/// a fetch failure ([`SkillError::Fetch`]), and rejects a snapshot that exceeds
/// the safety bounds or whose catalog has no supported root, an invalid manifest,
/// or a duplicate skill name ([`SkillError::Rejected`]).
pub fn validate_repo(
    fetcher: &dyn RepoFetcher,
    url: &str,
    staging: &Path,
) -> Result<ValidatedRepo, SkillError> {
    let normalized = normalize_url(url)?;
    // A clean staging dir; a stale one must never leak into a validation.
    let _ = std::fs::remove_dir_all(staging);
    let result = (|| {
        let snapshot = fetcher.fetch(&normalized, staging)?;
        ensure_snapshot_within_bounds(staging)?;
        let catalog = read_catalog(staging)?;
        Ok(ValidatedRepo {
            url: normalized.clone(),
            commit: snapshot.commit,
            catalog_root: catalog.root_label,
            packages: catalog.packages,
        })
    })();
    // Discovery leaves nothing behind: the staging tree is always removed.
    let _ = std::fs::remove_dir_all(staging);
    result
}

// --- review proposals -------------------------------------------------------

/// The lifecycle state of a review proposal. Discovery only ever writes
/// [`ProposalState::Pending`]; the review surface advances it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProposalState {
    /// Awaiting a reviewer decision.
    Pending,
    /// The reviewer deferred it; it stays for later without action.
    Deferred,
    /// The reviewer rejected it; discovery must not resurrect it.
    Rejected,
    /// The recommended skill (or its source) was acted on from the review surface.
    Acted,
}

/// A saved skill-discovery recommendation for human review. It records the full
/// evidence #41 requires — repository identity, catalog, ranked skills, the
/// primary recommendation, the query, the intended scope, timestamps, and
/// provenance — and is **not** a research finding or a memory candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillProposal {
    /// The normalized repository URL (the primary identity).
    pub repo_url: String,
    /// The resolved snapshot commit at discovery time.
    pub commit: String,
    /// The selected catalog root label.
    pub catalog_root: String,
    /// The skill names the repository offers.
    pub available_skills: Vec<String>,
    /// The primary recommended skill, if any.
    pub recommended_skill: Option<String>,
    /// Confidence of the recommendation in `0.0..=1.0`.
    pub confidence: f32,
    /// The rationale behind the recommendation.
    pub reason: String,
    /// The discovery query that surfaced this repository.
    pub query: String,
    /// The intended scope for a resulting registration/install (`project` / `global`).
    pub scope: String,
    /// Lifecycle state; discovery writes `Pending`.
    pub state: ProposalState,
    /// Where the repository was discovered (e.g. `github-search`, `mcp:server/tool`).
    pub provenance: String,
    /// First and last time this (repo, skill, scope) was seen (injected timestamps).
    pub first_seen: String,
    pub last_seen: String,
}

impl SkillProposal {
    /// The de-duplication identity: a repeated discovery of the same repository,
    /// recommended skill, and intended scope updates one proposal rather than
    /// inserting a duplicate.
    #[must_use]
    fn dedup_key(&self) -> (String, String, String) {
        (
            self.repo_url.clone(),
            self.recommended_skill.clone().unwrap_or_default(),
            self.scope.clone(),
        )
    }
}

/// The on-disk shape of the proposal store: a TOML file with a `[[proposal]]`
/// array.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ProposalFile {
    #[serde(default)]
    proposal: Vec<SkillProposal>,
}

/// The set of pending skill-discovery proposals, backed by a TOML file. Written
/// by the discovery lane and read by the review surface.
#[derive(Debug, Clone)]
pub struct ProposalStore {
    proposals: Vec<SkillProposal>,
    path: PathBuf,
}

impl ProposalStore {
    /// Load the store at `path`. A missing file is an empty store, not an error.
    ///
    /// # Errors
    /// Returns [`SkillError::Io`] if the file exists but cannot be read, or
    /// [`SkillError::Corrupt`] if it is not valid proposal TOML.
    pub fn load(path: &Path) -> Result<Self, SkillError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    proposals: Vec::new(),
                    path: path.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(SkillError::Io {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        let parsed: ProposalFile =
            toml::from_str(&text).map_err(|e| SkillError::Corrupt(e.to_string()))?;
        Ok(Self {
            proposals: parsed.proposal,
            path: path.to_path_buf(),
        })
    }

    /// The stored proposals, in insertion order.
    #[must_use]
    pub fn proposals(&self) -> &[SkillProposal] {
        &self.proposals
    }

    /// Insert `proposal`, or, if one with the same (repo, skill, scope) identity
    /// already exists, refresh its evidence and `last_seen` in place instead of
    /// creating a duplicate. A repeated discovery therefore updates the existing
    /// pending recommendation rather than piling up copies. `first_seen`, `state`,
    /// and a terminal decision (`Rejected`/`Acted`) on the existing proposal are
    /// preserved — discovery never resurrects a decided proposal.
    pub fn upsert(&mut self, proposal: SkillProposal) {
        let key = proposal.dedup_key();
        if let Some(existing) = self.proposals.iter_mut().find(|p| p.dedup_key() == key) {
            existing.commit = proposal.commit;
            existing.catalog_root = proposal.catalog_root;
            existing.available_skills = proposal.available_skills;
            existing.confidence = proposal.confidence;
            existing.reason = proposal.reason;
            existing.query = proposal.query;
            existing.provenance = proposal.provenance;
            existing.last_seen = proposal.last_seen;
        } else {
            self.proposals.push(proposal);
        }
    }

    /// Persist the store to its file, creating the parent directory if needed.
    ///
    /// # Errors
    /// Returns [`SkillError::Io`] if the directory or file cannot be written, or
    /// [`SkillError::Corrupt`] if serialization fails.
    pub fn save(&self) -> Result<(), SkillError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| SkillError::Io {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let file = ProposalFile {
            proposal: self.proposals.clone(),
        };
        let text = toml::to_string_pretty(&file)
            .map_err(|e| SkillError::Corrupt(format!("could not serialize proposals: {e}")))?;
        std::fs::write(&self.path, text).map_err(|source| SkillError::Io {
            path: self.path.display().to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::fetch::Snapshot;

    fn skill(name: &str, description: &str, state: MatchState) -> DiscoveredSkill {
        DiscoveredSkill {
            name: name.to_string(),
            description: description.to_string(),
            state,
            repo_url: None,
            source_id: None,
            commit: None,
            catalog_root: None,
            source_path: None,
            discoverable: false,
        }
    }

    #[test]
    fn ranking_orders_by_term_overlap_and_is_order_independent() {
        let a = skill(
            "threejs-webgl",
            "procedural materials for three.js",
            MatchState::Discovered,
        );
        let b = skill("gardening", "water the plants", MatchState::Discovered);
        let c = skill(
            "materials-lab",
            "study procedural materials",
            MatchState::Discovered,
        );
        let one = rank(
            "threejs procedural materials",
            vec![a.clone(), b.clone(), c.clone()],
            None,
        );
        let two = rank("threejs procedural materials", vec![c, b, a], None);
        // Same winner and same order regardless of input order.
        assert_eq!(one.matches[0].skill.name, "threejs-webgl");
        assert_eq!(
            one.matches
                .iter()
                .map(|m| m.skill.name.clone())
                .collect::<Vec<_>>(),
            two.matches
                .iter()
                .map(|m| m.skill.name.clone())
                .collect::<Vec<_>>()
        );
        let rec = one.recommendation.unwrap();
        assert_eq!(rec.name, "threejs-webgl");
        assert_eq!(rec.source, RecommendationSource::Deterministic);
        assert!(!rec.reason.is_empty());
    }

    #[test]
    fn a_non_matching_query_yields_no_recommendation() {
        let a = skill("gardening", "water the plants", MatchState::Available);
        let ranked = rank("kubernetes operators", vec![a], None);
        assert!(ranked.recommendation.is_none());
        assert_eq!(ranked.matches[0].score, 0.0);
    }

    #[test]
    fn a_model_classifier_can_refine_the_primary_pick_but_only_to_a_candidate() {
        struct PickSecond;
        impl SkillClassifier for PickSecond {
            fn recommend(&self, _q: &str, c: &[RankedSkill]) -> Option<Recommendation> {
                c.get(1).map(|m| Recommendation {
                    name: m.skill.name.clone(),
                    confidence: 0.9,
                    reason: "model preferred the runner-up".to_string(),
                    source: RecommendationSource::Model,
                })
            }
        }
        struct PickGhost;
        impl SkillClassifier for PickGhost {
            fn recommend(&self, _q: &str, _c: &[RankedSkill]) -> Option<Recommendation> {
                Some(Recommendation {
                    name: "does-not-exist".to_string(),
                    confidence: 1.0,
                    reason: "hallucinated".to_string(),
                    source: RecommendationSource::Model,
                })
            }
        }
        let a = skill(
            "threejs-webgl",
            "three.js materials",
            MatchState::Discovered,
        );
        let b = skill("materials-lab", "materials study", MatchState::Discovered);
        let refined = rank("materials", vec![a.clone(), b.clone()], Some(&PickSecond));
        assert_eq!(
            refined.recommendation.as_ref().unwrap().source,
            RecommendationSource::Model
        );
        // A classifier naming a non-candidate is ignored; the baseline stands.
        let guarded = rank("materials", vec![a, b], Some(&PickGhost));
        assert_eq!(
            guarded.recommendation.as_ref().unwrap().source,
            RecommendationSource::Deterministic
        );
    }

    /// A fetcher that lays down a fixture tree, for read-only validation tests.
    struct FakeFetcher {
        fixture: PathBuf,
        commit: String,
    }
    impl RepoFetcher for FakeFetcher {
        fn fetch(&self, _url: &str, dest: &Path) -> Result<Snapshot, SkillError> {
            copy_tree(&self.fixture, dest);
            Ok(Snapshot {
                commit: self.commit.clone(),
            })
        }
    }
    fn copy_tree(src: &Path, dst: &Path) {
        std::fs::create_dir_all(dst).unwrap();
        for entry in std::fs::read_dir(src).unwrap().flatten() {
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if from.is_dir() {
                copy_tree(&from, &to);
            } else {
                std::fs::copy(&from, &to).unwrap();
            }
        }
    }
    fn fixture_repo(root: &Path, names: &[&str]) {
        for name in names {
            let dir = root.join(".localpilot").join("skills").join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: does {name}\n---\nBody.\n"),
            )
            .unwrap();
        }
    }

    #[test]
    fn validate_repo_reads_the_catalog_without_leaving_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fixture");
        fixture_repo(&fixture, &["threejs-webgl", "other"]);
        let fetcher = FakeFetcher {
            fixture,
            commit: "c0ffee1234".to_string(),
        };
        let staging = tmp.path().join("staging");
        let repo = validate_repo(&fetcher, "https://github.com/owner/repo", &staging).unwrap();
        assert_eq!(repo.url, "https://github.com/owner/repo");
        assert_eq!(repo.commit, "c0ffee1234");
        assert_eq!(repo.catalog_root, ".localpilot/skills");
        assert!(repo.packages.iter().any(|p| p.name == "threejs-webgl"));
        // Read-only: the staging tree is gone after validation.
        assert!(!staging.exists(), "validation left a snapshot behind");
    }

    #[test]
    fn validate_repo_rejects_a_non_https_url_before_fetching() {
        let tmp = tempfile::tempdir().unwrap();
        let fetcher = FakeFetcher {
            fixture: tmp.path().join("nope"),
            commit: "x".to_string(),
        };
        let err =
            validate_repo(&fetcher, "git@github.com:o/r.git", &tmp.path().join("s")).unwrap_err();
        assert!(matches!(err, SkillError::Rejected(_)), "got {err:?}");
    }

    #[test]
    fn validate_repo_rejects_a_repository_without_a_supported_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture = tmp.path().join("fixture");
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(fixture.join("README.md"), "not a skill").unwrap();
        let fetcher = FakeFetcher {
            fixture,
            commit: "x".to_string(),
        };
        let err =
            validate_repo(&fetcher, "https://github.com/o/r", &tmp.path().join("s")).unwrap_err();
        assert!(matches!(err, SkillError::Rejected(_)), "got {err:?}");
    }

    fn proposal(repo: &str, skill: &str, scope: &str, seen: &str) -> SkillProposal {
        SkillProposal {
            repo_url: repo.to_string(),
            commit: format!("commit-{seen}"),
            catalog_root: ".localpilot/skills".to_string(),
            available_skills: vec![skill.to_string()],
            recommended_skill: Some(skill.to_string()),
            confidence: 0.8,
            reason: "matched terms".to_string(),
            query: "q".to_string(),
            scope: scope.to_string(),
            state: ProposalState::Pending,
            provenance: "github-search".to_string(),
            first_seen: seen.to_string(),
            last_seen: seen.to_string(),
        }
    }

    #[test]
    fn upsert_deduplicates_by_repo_skill_scope_and_updates_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("skill-proposals.toml");
        let mut store = ProposalStore::load(&path).unwrap();
        store.upsert(proposal(
            "https://github.com/o/r",
            "threejs-webgl",
            "project",
            "1000",
        ));
        // A repeat discovery of the same (repo, skill, scope) updates in place.
        store.upsert(proposal(
            "https://github.com/o/r",
            "threejs-webgl",
            "project",
            "2000",
        ));
        assert_eq!(store.proposals().len(), 1, "duplicate was not merged");
        let p = &store.proposals()[0];
        assert_eq!(p.first_seen, "1000", "first_seen is preserved");
        assert_eq!(p.last_seen, "2000", "last_seen is refreshed");
        assert_eq!(p.commit, "commit-2000", "evidence is refreshed");

        // A different scope is a distinct proposal.
        store.upsert(proposal(
            "https://github.com/o/r",
            "threejs-webgl",
            "global",
            "3000",
        ));
        assert_eq!(store.proposals().len(), 2);

        store.save().unwrap();
        let reloaded = ProposalStore::load(&path).unwrap();
        assert_eq!(reloaded.proposals().len(), 2);
    }
}
