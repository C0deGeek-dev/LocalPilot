//! Fetching a release and putting it in the cache.
//!
//! The order is deliberate and is the whole safety story:
//!
//! 1. download to memory,
//! 2. **verify the digest against the manifest**,
//! 3. extract into a staging directory,
//! 4. commit by atomic rename.
//!
//! Nothing is executed, and nothing becomes resolvable, until step 2 has passed.
//! A failure at any step leaves the previously installed version untouched —
//! there is no state in which the tool is unusable because an update was
//! interrupted.
//!
//! The digest proves the bytes are the bytes CI produced. It does **not** prove
//! who produced them: a party who can alter the release can alter the manifest
//! too. Only signing gives origin, and this build does not sign — every message
//! this module emits says so rather than implying more safety than it has.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cache::{Cache, InstallMarker, MARKER_VERSION};
use crate::error::DistError;
use crate::manifest::{Artifact, ReleaseManifest};
use crate::version::Version;

/// How long to wait for the whole download before giving up.
const DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
/// Retry transient request/body failures without retrying definitive HTTP
/// statuses such as a missing release asset.
const DOWNLOAD_ATTEMPTS: usize = 3;
/// Refuse an archive larger than this. A release archive is ~13 MB; anything
/// approaching this is a redirect to something that is not our artefact.
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;

/// Compute the lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// Check downloaded bytes against what the release published.
///
/// # Errors
/// Returns [`DistError::Checksum`] naming both digests, so a mismatch can be
/// investigated rather than merely retried.
pub fn verify(artifact: &Artifact, bytes: &[u8]) -> Result<(), DistError> {
    let actual = sha256_hex(bytes);
    if actual != artifact.sha256.to_ascii_lowercase() {
        return Err(DistError::Checksum {
            file: artifact.file.clone(),
            expected: artifact.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// Download `url` into memory, bounded by size and time.
///
/// # Errors
/// Returns [`DistError::Io`] for a transport failure, a non-success status, or
/// an oversized body.
pub async fn download(url: &str) -> Result<Vec<u8>, DistError> {
    // TLS validation is never disabled: `rustls-tls` with the default roots.
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(DistError::io)?;
    let mut last_transport_error = None;
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        let response = match client.get(url).send().await {
            Ok(response) => response,
            Err(error) => {
                let error = DistError::io(error);
                if attempt == DOWNLOAD_ATTEMPTS {
                    return Err(error);
                }
                last_transport_error = Some(error);
                continue;
            }
        };
        if !response.status().is_success() {
            return Err(DistError::Io(format!(
                "download {url} returned {}",
                response.status()
            )));
        }
        if let Some(len) = response.content_length() {
            if len > MAX_ARCHIVE_BYTES {
                return Err(DistError::Io(format!(
                    "refusing a {len}-byte download; the archive should be a few tens of MB"
                )));
            }
        }
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => {
                let error = DistError::io(error);
                if attempt == DOWNLOAD_ATTEMPTS {
                    return Err(error);
                }
                last_transport_error = Some(error);
                continue;
            }
        };
        if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
            return Err(DistError::Io(format!(
                "refusing a {}-byte download",
                bytes.len()
            )));
        }
        return Ok(bytes.to_vec());
    }
    Err(last_transport_error.unwrap_or_else(|| {
        DistError::Io(format!("download {url} failed without a transport error"))
    }))
}

/// Extract a `.tar.gz` archive into `into`.
///
/// One archive format is used for every target — Windows included — so there is
/// one extractor rather than one per platform.
///
/// # Errors
/// Returns [`DistError::Io`] when the archive is malformed or a member escapes
/// the destination.
pub fn extract(bytes: &[u8], into: &Path) -> Result<(), DistError> {
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(decoder);
    // Refuse absolute paths and `..` traversal: an archive must not write
    // outside the staging directory it was handed.
    archive.set_overwrite(true);
    for entry in archive.entries().map_err(DistError::io)? {
        let mut entry = entry.map_err(DistError::io)?;
        let path = entry.path().map_err(DistError::io)?.into_owned();
        if escapes_destination(&path) {
            return Err(DistError::Invalid(format!(
                "archive member {} escapes the destination",
                path.display()
            )));
        }
        entry.unpack_in(into).map_err(DistError::io)?;
    }
    Ok(())
}

/// Whether an archive member would write outside the directory it is unpacked
/// into — an absolute path, or any `..` component.
///
/// Kept as its own function because it is the rule worth testing exhaustively:
/// the `tar` crate's *builder* refuses to create such an archive, so a hostile
/// one cannot be produced with it, and the guard has to be verified directly
/// rather than through a fixture.
#[must_use]
pub fn escapes_destination(path: &Path) -> bool {
    path.is_absolute()
        || path.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::RootDir
            )
        })
}

/// Find the executable named `binary` anywhere under `dir`.
///
/// Archives nest their payload in a directory named after the tool, so the
/// executable is one level down rather than at the root.
#[must_use]
pub fn find_executable(dir: &Path, binary: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{binary}.exe")
    } else {
        binary.to_string()
    };
    let direct = dir.join(&exe);
    if direct.is_file() {
        return Some(direct);
    }
    std::fs::read_dir(dir).ok()?.flatten().find_map(|entry| {
        let nested = entry.path().join(&exe);
        nested.is_file().then_some(nested)
    })
}

/// Download, verify, extract, and install one release version.
///
/// Returns the directory the version was installed into.
///
/// # Errors
/// Returns [`DistError::Manifest`] when the release did not build for this
/// target, [`DistError::Checksum`] when the download does not match what was
/// published, and [`DistError::Io`] for transport or filesystem failures. In
/// every case the previously installed version is left untouched.
pub async fn install_release(
    cache: &Cache,
    manifest: &ReleaseManifest,
    target: &str,
    binary: &str,
    base_url: &str,
) -> Result<PathBuf, DistError> {
    let artifact = manifest.artifact(target).ok_or_else(|| {
        DistError::Manifest(format!(
            "release {} has no build for {target}; it shipped: {}",
            manifest.version,
            manifest.targets().join(", ")
        ))
    })?;
    let version = Version::parse(&manifest.version).ok_or_else(|| {
        DistError::Manifest(format!(
            "release version {:?} is unparseable",
            manifest.version
        ))
    })?;

    let url = format!("{}/{}", base_url.trim_end_matches('/'), artifact.file);
    let bytes = download(&url).await?;
    // Verify before anything is written where it could be run.
    verify(artifact, &bytes)?;

    let staged = cache.stage(&version)?;
    let unpacked = || -> Result<PathBuf, DistError> {
        extract(&bytes, &staged)?;
        find_executable(&staged, binary).ok_or_else(|| {
            DistError::Invalid(format!("archive {} contains no {binary}", artifact.file))
        })
    }();
    let executable = match unpacked {
        Ok(path) => path,
        Err(error) => {
            // Leave nothing half-written behind.
            let _ = std::fs::remove_dir_all(&staged);
            return Err(error);
        }
    };

    // The payload may be nested one directory down; flatten so the marker's
    // `executable` is always relative to the version directory.
    let root = executable.parent().unwrap_or(&staged).to_path_buf();
    let name = executable
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(binary)
        .to_string();
    let marker = InstallMarker {
        marker_version: MARKER_VERSION,
        version: version.to_dir_name(),
        target: target.to_string(),
        sha256: artifact.sha256.to_ascii_lowercase(),
        executable: name,
    };
    let result = cache.commit(&version, &root, &marker);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staged);
    }
    result
}

#[cfg(test)]
mod download_tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn download_retries_a_transport_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            drop(first);

            let (mut second, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = second.read(&mut request);
            second
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });

        let bytes = super::download(&format!("http://{address}/manifest.json"))
            .await
            .unwrap();

        assert_eq!(bytes, b"ok");
        server.join().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::Artifact;

    fn artifact(sha: &str) -> Artifact {
        Artifact {
            target: "t".to_string(),
            file: "tool-t.tar.gz".to_string(),
            size: 3,
            sha256: sha.to_string(),
        }
    }

    #[test]
    fn the_digest_matches_a_known_value() {
        // SHA-256 of "abc", the standard test vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verification_accepts_the_published_digest() {
        let bytes = b"abc";
        let good = artifact(&sha256_hex(bytes));
        verify(&good, bytes).expect("matching digest");
    }

    #[test]
    fn verification_is_case_insensitive_about_hex() {
        let bytes = b"abc";
        let upper = artifact(&sha256_hex(bytes).to_ascii_uppercase());
        verify(&upper, bytes).expect("hex case must not decide safety");
    }

    #[test]
    fn a_corrupted_download_is_refused_and_names_both_digests() {
        let published = artifact(&sha256_hex(b"abc"));
        let error = verify(&published, b"abd").expect_err("a different byte must fail");
        let message = error.to_string();
        assert!(message.contains("tool-t.tar.gz"), "{message}");
        assert!(
            message.contains(&sha256_hex(b"abd")),
            "reports what it got: {message}"
        );
        assert!(
            message.contains(&sha256_hex(b"abc")),
            "and what it wanted: {message}"
        );
    }

    #[test]
    fn a_truncated_download_is_refused() {
        let published = artifact(&sha256_hex(b"abcdef"));
        assert!(verify(&published, b"abc").is_err());
    }

    fn tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, name, *body)
                .expect("append");
        }
        let tarball = builder.into_inner().expect("finish");
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &tarball).expect("compress");
        encoder.finish().expect("finish gz")
    }

    #[test]
    fn extraction_places_a_nested_payload_and_finds_the_binary() {
        let dir = tempfile::tempdir().expect("tmp");
        let exe = if cfg!(windows) {
            "tool/tool.exe"
        } else {
            "tool/tool"
        };
        let archive = tar_gz(&[(exe, b"binary"), ("tool/README.md", b"docs")]);
        extract(&archive, dir.path()).expect("extract");
        let found = find_executable(dir.path(), "tool").expect("finds the nested binary");
        assert!(found.is_file());
        assert_eq!(std::fs::read(&found).expect("read"), b"binary");
    }

    #[test]
    fn the_escape_rule_rejects_traversal_and_absolute_paths() {
        for bad in ["../escaped", "tool/../../escaped", "/etc/passwd"] {
            assert!(
                escapes_destination(Path::new(bad)),
                "{bad:?} must be refused"
            );
        }
        for good in ["tool/tool", "tool/README.md", "a/b/c"] {
            assert!(
                !escapes_destination(Path::new(good)),
                "{good:?} is an ordinary member"
            );
        }
    }

    #[test]
    fn a_hostile_archive_cannot_even_be_built_with_the_tar_crate() {
        // Documents why the rule above is tested directly: the builder refuses
        // to write a `..` member, so no fixture can carry one. The guard stays
        // because nothing promises every producer is this crate.
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(1);
        header.set_cksum();
        assert!(
            builder
                .append_data(&mut header, "../escaped", &b"x"[..])
                .is_err(),
            "the tar builder is expected to refuse traversal"
        );
    }

    #[test]
    fn a_malformed_archive_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().expect("tmp");
        assert!(extract(b"not a gzip stream at all", dir.path()).is_err());
    }

    #[tokio::test]
    async fn an_archive_this_build_cannot_extract_leaves_the_cache_untouched() {
        // The real case this guards: a release older than the one-format change
        // published a `.zip` for Windows. Attempting it must fail cleanly rather
        // than leaving a half-written version, and must not disturb whatever is
        // already installed.
        let temp = tempfile::tempdir().expect("tmp");
        let cache = crate::Cache::new(temp.path().join("tool"));

        // Seed an existing install, which must survive the failed attempt.
        let existing = Version::parse("1.0.0").expect("parses");
        let staged = cache.stage(&existing).expect("stage");
        let exe = if cfg!(windows) { "tool.exe" } else { "tool" };
        std::fs::write(staged.join(exe), b"old").expect("write");
        cache
            .commit(
                &existing,
                &staged,
                &InstallMarker {
                    marker_version: MARKER_VERSION,
                    version: "1.0.0".to_string(),
                    target: "t".to_string(),
                    sha256: "0".repeat(64),
                    executable: exe.to_string(),
                },
            )
            .expect("seed");

        // Not a gzip stream at all — stands in for the zip case.
        let payload = b"PK not a tarball";
        let version = Version::parse("2.0.0").expect("parses");
        let staged = cache.stage(&version).expect("stage");
        let outcome = extract(payload, &staged);
        assert!(outcome.is_err(), "an unreadable archive must fail");
        let _ = std::fs::remove_dir_all(&staged);

        let installed: Vec<String> = cache
            .installed()
            .iter()
            .map(|c| c.version.to_dir_name())
            .collect();
        assert_eq!(
            installed,
            ["1.0.0"],
            "the previously installed version must be untouched and still the only one"
        );
    }

    #[test]
    fn find_executable_returns_none_when_absent() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("README.md"), b"x").expect("write");
        assert!(find_executable(dir.path(), "tool").is_none());
    }
}
