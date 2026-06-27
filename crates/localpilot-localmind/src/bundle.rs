//! Host surface for portable, signed memory bundles.
//!
//! Wraps the LocalMind store's export/sign and verify/import into plain,
//! LocalPilot-owned summary types so the CLI never names a LocalMind type. The
//! round-trip is the same one the `localmind` CLI exposes; on LocalPilot it is
//! surfaced under `learning export` / `learning import` (the lesson surface),
//! because `localpilot memory export` is the code-graph snapshot.

use std::path::Path;

use localmind_store::{
    author_fingerprint, sign_bundle, BundleImporter, BundleScope, ImportTrust, KeyStore,
    MemoryBundleExporter, SignedBundle,
};

use crate::error::LearningError;

/// The outcome of exporting a signed bundle.
#[derive(Debug, Clone)]
pub struct BundleExportSummary {
    /// Where the signed bundle was written.
    pub output: String,
    /// How many accepted memories were exported.
    pub entries: usize,
    /// How many apparent secrets were redacted before export.
    pub redactions: usize,
    /// The signing author fingerprint.
    pub author: String,
    /// The bundle content digest.
    pub digest: String,
}

/// The outcome of importing a signed bundle.
#[derive(Debug, Clone)]
pub struct BundleImportSummary {
    /// `trusted`, `untrusted`, or `rejected`.
    pub trust: String,
    /// Why a rejected bundle was rejected (when `trust == "rejected"`).
    pub rejected_reason: Option<String>,
    /// Entries in the bundle.
    pub total: usize,
    /// Entries newly enqueued for review (or that would be, on a dry run).
    pub added: usize,
    /// Entries collapsed by dedup.
    pub duplicate: usize,
    /// Entries not imported because the bundle was rejected.
    pub rejected: usize,
    /// Whether changes were written (`false` for a dry run).
    pub applied: bool,
}

fn parse_scope(scope: &str) -> Result<BundleScope, LearningError> {
    match scope {
        "project" => Ok(BundleScope::Project),
        "global" => Ok(BundleScope::Global),
        "both" => Ok(BundleScope::Both),
        other => Err(LearningError::Bundle(format!(
            "unknown scope {other:?} (use project|global|both)"
        ))),
    }
}

/// Export accepted memory (of `scope`) to a signed bundle at `out`.
///
/// # Errors
/// Returns [`LearningError::Bundle`] if the store cannot be read, the signing key
/// cannot be loaded/created, or the file cannot be written.
pub fn bundle_export(
    project_root: &Path,
    scope: &str,
    out: &Path,
) -> Result<BundleExportSummary, LearningError> {
    let scope = parse_scope(scope)?;
    let exporter = MemoryBundleExporter::open_project(project_root)
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    let signing_key = KeyStore::open(project_root)
        .and_then(|store| store.load_or_generate())
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    let author = author_fingerprint(&signing_key.verifying_key().to_bytes());
    let outcome = exporter
        .export(scope, &author)
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    let signed = sign_bundle(&outcome.bundle, &signing_key)
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    let json = signed
        .to_pretty_json()
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    std::fs::write(out, json).map_err(|e| LearningError::Bundle(e.to_string()))?;
    Ok(BundleExportSummary {
        output: out.display().to_string(),
        entries: outcome.bundle.entries.len(),
        redactions: outcome.scan.redactions,
        author,
        digest: signed.signature.digest,
    })
}

/// Verify and import a signed bundle from `input`. With `apply = false` (the
/// default) it is a dry run that writes nothing; with `apply = true` the entries
/// are enqueued for review (never promoted to active memory).
///
/// # Errors
/// Returns [`LearningError::Bundle`] if the file cannot be read/parsed or the
/// import fails. A *rejected* bundle is reported, not errored.
pub fn bundle_import(
    project_root: &Path,
    input: &Path,
    apply: bool,
) -> Result<BundleImportSummary, LearningError> {
    let text = std::fs::read_to_string(input).map_err(|e| LearningError::Bundle(e.to_string()))?;
    let signed =
        SignedBundle::from_json(&text).map_err(|e| LearningError::Bundle(e.to_string()))?;
    let report = BundleImporter::new(project_root)
        .import(&signed, apply)
        .map_err(|e| LearningError::Bundle(e.to_string()))?;
    let (trust, rejected_reason) = match report.trust {
        ImportTrust::Trusted => ("trusted".to_string(), None),
        ImportTrust::Untrusted => ("untrusted".to_string(), None),
        ImportTrust::Rejected(reason) => ("rejected".to_string(), Some(reason)),
    };
    Ok(BundleImportSummary {
        trust,
        rejected_reason,
        total: report.total,
        added: report.added,
        duplicate: report.duplicate,
        rejected: report.rejected,
        applied: report.applied,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::seed::{seed_memory, SeedLesson};

    fn project(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let root = dir.path().to_path_buf();
        let global = root.join("global-store").join("memory");
        std::fs::write(
            root.join(".localmind.toml"),
            format!(
                "[learning]\nenabled = true\nglobal_memory_root = {:?}\n",
                global.to_string_lossy()
            ),
        )
        .unwrap();
        root
    }

    fn lesson(body: &str) -> SeedLesson {
        SeedLesson {
            body: body.to_string(),
            category: Some("ProjectConvention".to_string()),
            confidence: Some(0.8),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn host_surface_round_trips_export_then_import() {
        // Machine A: seed accepted memory, export a signed bundle.
        let dir_a = tempfile::tempdir().unwrap();
        let root_a = project(&dir_a);
        seed_memory(
            &root_a,
            &[
                lesson("prefer ripgrep over grep when searching the codebase"),
                lesson("run the integration suite after an exporter change"),
            ],
            false,
        )
        .unwrap();
        let out = dir_a.path().join("pack.json");
        let export = bundle_export(&root_a, "both", &out).unwrap();
        assert_eq!(export.entries, 2);
        assert!(out.exists());

        // Machine B: dry run writes nothing; --apply enqueues for review.
        let dir_b = tempfile::tempdir().unwrap();
        let root_b = project(&dir_b);
        let dry = bundle_import(&root_b, &out, false).unwrap();
        assert_eq!(dry.trust, "untrusted");
        assert_eq!(dry.added, 2);
        assert!(!dry.applied);
        assert!(crate::ops::review_list(&root_b).unwrap().is_empty());

        let applied = bundle_import(&root_b, &out, true).unwrap();
        assert!(applied.applied);
        assert_eq!(applied.added, 2);
        assert_eq!(crate::ops::review_list(&root_b).unwrap().len(), 2);
    }

    #[test]
    fn an_unknown_scope_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(&dir);
        assert!(bundle_export(&root, "everything", &dir.path().join("x.json")).is_err());
    }
}
