//! What each installed tool was built from, recorded where the install happens.
//!
//! A version string does not answer this. Three of the five tools stamp their
//! crate version and nothing else, so a development build of `localmind` from a
//! working tree and the published release of the same version are the same four
//! characters on screen — and in development mode, "is this actually my code?"
//! is the question being asked. The channel line in `localx status` answers it
//! for the *next* install, not for what is on disk.
//!
//! So each install writes one line of provenance into `<localx root>/installed.json`,
//! and `status` reads it back. The record is advisory: a missing or unreadable
//! entry means the row prints exactly as it did before.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use localpilot_dist::Cache;

/// The record's file name, beside the workspace pin in the `localx` data
/// directory.
const RECORD_FILE: &str = "installed.json";

/// The record's format version, so a future shape change is a recognised
/// mismatch rather than a silent misread.
const RECORD_VERSION: u32 = 1;

/// Where a tool on disk came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The published archive for a release tag.
    Release { tag: String },
    /// A build of a repository's pushed `main`.
    Prerelease,
    /// A build of the working trees in a local workspace.
    Workspace { path: PathBuf },
}

impl Origin {
    /// One phrase for a status row, in the reader's terms rather than the
    /// enum's.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Release { tag } => format!("release {tag}"),
            Self::Prerelease => "built from main".to_string(),
            Self::Workspace { path } => format!("development build from {}", path.display()),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Release { tag } => serde_json::json!({"channel": "release", "tag": tag}),
            Self::Prerelease => serde_json::json!({"channel": "prerelease"}),
            Self::Workspace { path } => serde_json::json!({
                "channel": "workspace",
                "workspace": path.display().to_string(),
            }),
        }
    }

    fn from_json(value: &serde_json::Value) -> Option<Self> {
        match value.get("channel")?.as_str()? {
            "release" => Some(Self::Release {
                tag: value.get("tag")?.as_str()?.to_string(),
            }),
            "prerelease" => Some(Self::Prerelease),
            "workspace" => Some(Self::Workspace {
                path: PathBuf::from(value.get("workspace")?.as_str()?),
            }),
            _ => None,
        }
    }
}

/// Record that `tool` now holds a build from `origin`.
///
/// Best effort in both directions: a platform with no data directory records
/// nothing, and a write that fails changes nothing a caller should act on — the
/// install itself succeeded, and this only decides how a later `status` reads.
pub fn record(tool: &str, origin: &Origin) {
    if let Some(root) = Cache::default_root("localx") {
        record_in(&root, tool, origin);
    }
}

/// What `tool` was last installed from, when that is known.
#[must_use]
pub fn origin(tool: &str) -> Option<Origin> {
    read_from(&Cache::default_root("localx")?).remove(tool)
}

fn record_in(root: &Path, tool: &str, origin: &Origin) {
    let mut tools = serde_json::Map::new();
    for (name, existing) in read_from(root) {
        tools.insert(name, existing.to_json());
    }
    tools.insert(tool.to_string(), origin.to_json());
    let document = serde_json::json!({
        "version": RECORD_VERSION,
        "tools": serde_json::Value::Object(tools),
    });
    if std::fs::create_dir_all(root).is_ok() {
        let _ = std::fs::write(root.join(RECORD_FILE), format!("{document:#}\n"));
    }
}

fn read_from(root: &Path) -> BTreeMap<String, Origin> {
    let Ok(text) = std::fs::read_to_string(root.join(RECORD_FILE)) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return BTreeMap::new();
    };
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(RECORD_VERSION)) {
        return BTreeMap::new();
    }
    value
        .get("tools")
        .and_then(serde_json::Value::as_object)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|(name, entry)| {
                    Origin::from_json(entry).map(|origin| (name.clone(), origin))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{read_from, record_in, Origin};
    use std::path::PathBuf;

    #[test]
    fn each_origin_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        record_in(
            root,
            "localbox",
            &Origin::Release {
                tag: "v5.0.0".into(),
            },
        );
        record_in(root, "localmind", &Origin::Prerelease);
        record_in(
            root,
            "localpilot",
            &Origin::Workspace {
                path: PathBuf::from("/repos/LocalX"),
            },
        );

        let read = read_from(root);
        assert_eq!(read.len(), 3);
        assert_eq!(
            read["localbox"],
            Origin::Release {
                tag: "v5.0.0".into()
            }
        );
        assert_eq!(read["localmind"], Origin::Prerelease);
        assert_eq!(
            read["localpilot"],
            Origin::Workspace {
                path: PathBuf::from("/repos/LocalX")
            }
        );
    }

    #[test]
    fn a_later_install_replaces_only_its_own_tool() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        record_in(root, "localbox", &Origin::Prerelease);
        record_in(root, "localmind", &Origin::Prerelease);
        record_in(
            root,
            "localbox",
            &Origin::Release {
                tag: "v5.0.0".into(),
            },
        );

        let read = read_from(root);
        assert_eq!(
            read["localbox"],
            Origin::Release {
                tag: "v5.0.0".into()
            }
        );
        assert_eq!(read["localmind"], Origin::Prerelease);
    }

    #[test]
    fn an_unreadable_or_foreign_record_reads_as_nothing_known() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(read_from(root).is_empty());

        std::fs::write(root.join(super::RECORD_FILE), "not json").unwrap();
        assert!(read_from(root).is_empty());

        // A future format version is recognised as unreadable rather than
        // guessed at.
        std::fs::write(
            root.join(super::RECORD_FILE),
            r#"{"version": 99, "tools": {"localbox": {"channel": "prerelease"}}}"#,
        )
        .unwrap();
        assert!(read_from(root).is_empty());
    }

    #[test]
    fn a_description_says_what_a_reader_needs() {
        assert_eq!(
            Origin::Release {
                tag: "v5.0.0".into()
            }
            .describe(),
            "release v5.0.0"
        );
        assert_eq!(Origin::Prerelease.describe(), "built from main");
        assert!(Origin::Workspace {
            path: PathBuf::from("/repos/LocalX")
        }
        .describe()
        .starts_with("development build from "));
    }
}
