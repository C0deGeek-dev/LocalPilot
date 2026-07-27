//! Opt-in live check against a real published release.
//!
//! Ignored by default because it reaches the network. Run it deliberately:
//!
//! ```text
//! cargo test -p localpilot-dist --test live_release -- --ignored --nocapture
//! ```
//!
//! What it proves that a unit test cannot: that the bytes GitHub actually serves
//! match the digest the release published, that the archive CI produced extracts
//! with this extractor, and that the result commits into the cache and resolves.
//! Every one of those is a contract with something outside this repository.

use localpilot_dist::{Cache, ReleaseManifest, Version};

/// A published release known to carry the manifest format this build reads.
///
/// Pinned rather than tracking "latest" so a failure means *this build* stopped
/// reading a release it used to read — not that someone published something new.
/// Bump it deliberately when the manifest format or the archive layout changes.
const BASE: &str = "https://github.com/C0deGeek-dev/LocalPilot/releases/download/v2.6.0";
/// A Linux archive is used regardless of host: extraction is platform-agnostic,
/// and this test is about the distribution mechanics, not about running the
/// binary. From 2.6.0 every target ships `.tar.gz`, so the choice is arbitrary.
const TARGET: &str = "x86_64-unknown-linux-musl";

#[tokio::test]
#[ignore = "reaches the network; run explicitly"]
async fn a_real_release_downloads_verifies_extracts_and_installs() {
    let manifest_text = localpilot_dist::download(&format!("{BASE}/manifest.json"))
        .await
        .expect("the release publishes a manifest");
    let manifest = ReleaseManifest::parse(&String::from_utf8(manifest_text).expect("utf-8"))
        .expect("the published manifest parses with this build's reader");
    println!(
        "manifest: {} {} ({} artefacts)",
        manifest.tool,
        manifest.version,
        manifest.artifacts.len()
    );

    let artifact = manifest
        .artifact(TARGET)
        .unwrap_or_else(|| panic!("release shipped {TARGET}; it has: {:?}", manifest.targets()));

    let bytes = localpilot_dist::download(&format!("{BASE}/{}", artifact.file))
        .await
        .expect("the archive downloads");
    println!("downloaded {} bytes", bytes.len());
    assert_eq!(
        bytes.len() as u64,
        artifact.size,
        "the served size must match what the manifest published"
    );

    // The load-bearing assertion: real served bytes against the real published
    // digest.
    localpilot_dist::verify(artifact, &bytes)
        .expect("the served bytes must match the published digest");
    println!("digest verified: {}", artifact.sha256);

    // And a corrupted copy must be refused, so the check is not vacuous.
    let mut corrupted = bytes.clone();
    corrupted[0] ^= 0xff;
    assert!(
        localpilot_dist::verify(artifact, &corrupted).is_err(),
        "a single flipped byte must fail verification"
    );

    let temp = tempfile::tempdir().expect("tmp");
    let cache = Cache::new(temp.path().join("localpilot"));
    let version = Version::parse(&manifest.version).expect("release version parses");

    let staged = cache.stage(&version).expect("stage");
    localpilot_dist::extract(&bytes, &staged).expect("the CI-built archive extracts");
    // Deliberately cross-target: this host is not necessarily Linux, so the
    // payload is located by name rather than through `find_executable`, which
    // correctly looks for the *host's* executable name.
    let executable = std::fs::read_dir(&staged)
        .expect("staged dir")
        .flatten()
        .find_map(|entry| {
            let candidate = entry.path().join("localpilot");
            candidate.is_file().then_some(candidate)
        })
        .expect("the archive contains the binary");
    println!("extracted: {}", executable.display());

    let root = executable.parent().expect("parent");
    let marker = localpilot_dist::InstallMarker {
        marker_version: localpilot_dist::MARKER_VERSION,
        version: version.to_dir_name(),
        target: TARGET.to_string(),
        sha256: artifact.sha256.clone(),
        executable: executable
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned(),
    };
    let installed = cache.commit(&version, root, &marker).expect("commit");
    println!("installed to {}", installed.display());

    // It is now the version the resolver would run, from an older build.
    let older = Version::parse("2.4.0").expect("parses");
    let resolution = localpilot_dist::resolve(&cache, &older);
    assert!(
        resolution.is_handoff(),
        "a newer install should be handed off to"
    );
    assert_eq!(resolution.version, version);
    println!(
        "resolved: {} ({})",
        resolution.version.to_dir_name(),
        resolution.reason.explain()
    );
}
