//! The vendored conformance suite is exactly what its manifest says: no file
//! changed, missing or added since it was copied from its source repository.
//! Refresh it with the source's `vendor.py`, never by hand.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn suite() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance")
}

/// The manifest hashes the LF form, so a checkout that rewrites line endings
/// still matches.
fn digest(p: &Path) -> String {
    let bytes = std::fs::read(p).unwrap();
    let mut lf = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\r' && bytes.get(i + 1) == Some(&b'\n') {
            i += 1;
            continue;
        }
        lf.push(bytes[i]);
        i += 1;
    }
    Sha256::digest(&lf)
        .iter()
        .fold(String::new(), |mut out, b| {
            use std::fmt::Write as _;
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn files_under(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            files_under(root, &p, out);
        } else {
            let rel = p
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.insert(rel);
        }
    }
}

#[test]
fn the_vendored_suite_matches_its_manifest() {
    let root = suite();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("MANIFEST.json")).unwrap())
            .unwrap();
    let files = manifest["files"].as_object().expect("manifest lists files");
    let mut drift = Vec::new();
    for (rel, want) in files {
        let p = root.join(rel);
        if !p.is_file() {
            drift.push(format!("missing {rel}"));
        } else if digest(&p) != want.as_str().unwrap_or_default() {
            drift.push(format!("changed {rel}"));
        }
    }
    let mut present = BTreeSet::new();
    files_under(&root, &root, &mut present);
    present.remove("MANIFEST.json");
    for rel in present {
        if !files.contains_key(&rel) {
            drift.push(format!("unlisted {rel}"));
        }
    }
    assert!(
        drift.is_empty(),
        "the vendored suite drifted from its manifest (source {}): {drift:?}",
        manifest["source_commit"]
    );
    assert!(
        files.keys().filter(|k| k.starts_with("fixtures/")).count() > 0,
        "the manifest lists no fixtures"
    );
}
