//! Detection of secret-like file paths.
//!
//! This guards reads/edits of credential-bearing files (`secret_file_guard`).
//! It is best-effort and matches on file names and well-known credential
//! locations, not contents.

use std::path::Path;

/// Whether a path looks like it holds secrets and should require approval to
/// read or edit under the `default` and `relaxed` profiles.
#[must_use]
pub fn is_secret_like(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    // Dotenv files: `.env`, `.env.local`, ... but not `.env.example`/`.env.sample`.
    if name == ".env"
        || (name.starts_with(".env") && !name.contains("example") && !name.contains("sample"))
    {
        return true;
    }

    const SECRET_NAMES: &[&str] = &[
        "id_rsa",
        "id_ed25519",
        "id_ecdsa",
        "id_dsa",
        ".npmrc",
        ".pypirc",
        ".netrc",
        "credentials",
        ".git-credentials",
        ".htpasswd",
        "secrets.yaml",
        "secrets.yml",
        "secrets.json",
        "kubeconfig",
    ];
    if SECRET_NAMES.contains(&name.as_str()) {
        return true;
    }

    if let Some(ext) = Path::new(&name).extension().and_then(|e| e.to_str()) {
        if matches!(ext, "pem" | "key" | "p12" | "pfx" | "keystore" | "jks") {
            return true;
        }
    }

    // Well-known credential directories anywhere in the path.
    let lossy = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    const SECRET_DIRS: &[&str] = &[".aws/", ".ssh/", ".gnupg/", ".kube/", ".docker/config.json"];
    SECRET_DIRS.iter().any(|d| lossy.contains(d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_credential_files() {
        assert!(is_secret_like(Path::new(".env")));
        assert!(is_secret_like(Path::new("config/.env.local")));
        assert!(is_secret_like(Path::new("deploy/server.pem")));
        assert!(is_secret_like(Path::new("keys/id_rsa")));
        assert!(is_secret_like(Path::new("/home/u/.aws/credentials")));
        assert!(is_secret_like(Path::new(".ssh/known_hosts")));
    }

    #[test]
    fn passes_ordinary_files() {
        assert!(!is_secret_like(Path::new("src/main.rs")));
        assert!(!is_secret_like(Path::new(".env.example")));
        assert!(!is_secret_like(Path::new("README.md")));
        assert!(!is_secret_like(Path::new("Cargo.toml")));
    }
}
