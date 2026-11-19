//! `[search]`: search backends that LocalPilot set up and manages itself.
//!
//! A managed backend is a search-only MCP server: nothing starts it at session
//! start, its tools never reach the model's tool list, and only chat
//! `web_search` and `/research` start it, on their first real search, to call
//! its one designated tool. It lives in its own top-level table rather than
//! under `[mcp.servers]` or `[research.mcp]`, so a binary that predates it sees
//! no server and no designation and stays inert.
//!
//! The table is read from the user's own config file only. A project file or
//! an environment variable that tries to set it is ignored and reported (see
//! [`crate::Config::search_ignored`]): a repository must not be able to choose
//! which program is started to send queries off the machine.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::schema::{McpEnvEntry, McpServerConfig};

/// `[search]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    /// `[search.managed.<id>]`: managed backends, tried in this order after
    /// any search tools designated by hand in `[research.mcp] tools`.
    pub managed: IndexMap<String, ManagedSearchBackend>,
}

/// Which kind of backend an entry is, for disclosure and audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchTier {
    /// A keyless public search package run on this machine.
    Keyless,
    /// A search engine the user hosts on this machine.
    SelfHosted,
    /// A hosted search API reached with the user's own key.
    ApiKey,
}

impl SearchTier {
    /// The stable name used in audit records and output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Keyless => "keyless",
            Self::SelfHosted => "self_hosted",
            Self::ApiKey => "api_key",
        }
    }

    /// Where queries sent through this kind of backend end up.
    #[must_use]
    pub const fn destinations(self) -> &'static str {
        match self {
            Self::Keyless => {
                "the package on this machine sends each query to a public search engine"
            }
            Self::SelfHosted => {
                "the search engine on this machine sends each query to the public search \
                 engines it is configured to ask"
            }
            Self::ApiKey => "each query goes to the hosted search API, with your key",
        }
    }
}

/// `[search.managed.<id>]`: one managed backend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedSearchBackend {
    /// What kind of backend this is.
    pub tier: SearchTier,
    /// The program that serves the MCP search tool.
    pub command: String,
    /// Its arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment entries for the program, in the `[mcp.servers]` forms.
    #[serde(default)]
    pub env: IndexMap<String, McpEnvEntry>,
    /// The one tool LocalPilot calls; the server's other tools are never used.
    pub tool: String,
    /// The launch identity recorded when the backend was set up (see
    /// [`launch_identity`]). A launch whose definition no longer matches it is
    /// refused.
    pub launch_identity: String,
    /// Longest wait for one search call, in seconds.
    #[serde(default = "default_call_timeout_secs")]
    pub call_timeout_secs: u64,
    /// Longest wait for the server to start and answer its handshake, in
    /// seconds. Longer than a call, because a first start can download.
    #[serde(default = "default_handshake_timeout_secs")]
    pub handshake_timeout_secs: u64,
    /// Longest query sent, in characters, after redaction.
    #[serde(default = "default_max_query_chars")]
    pub max_query_chars: usize,
    /// Longest query sent, in words, when the backend limits words.
    #[serde(default)]
    pub max_query_words: Option<usize>,
    /// Whether research nobody is watching (piped, scheduled, headless) may
    /// start this backend. Off unless the user said yes at setup.
    #[serde(default)]
    pub unattended: bool,
}

const fn default_call_timeout_secs() -> u64 {
    20
}

const fn default_handshake_timeout_secs() -> u64 {
    120
}

const fn default_max_query_chars() -> usize {
    400
}

impl ManagedSearchBackend {
    /// The launch definition as an MCP server entry, for the spawn path.
    #[must_use]
    pub fn server(&self) -> McpServerConfig {
        McpServerConfig {
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
        }
    }

    /// Whether the launch definition still matches what was recorded.
    #[must_use]
    pub fn identity_matches(&self) -> bool {
        launch_identity(&self.server()) == self.launch_identity
    }

    /// `query` cut to this backend's limits, at a word boundary.
    #[must_use]
    pub fn bound_query(&self, query: &str) -> String {
        bound_query(query, self.max_query_chars, self.max_query_words)
    }
}

/// A stable digest of what a launch would run: the command, its arguments and
/// each environment entry's name and form. A credential reference contributes
/// its alias; a literal contributes a digest of its value, never the value.
#[must_use]
pub fn launch_identity(server: &McpServerConfig) -> String {
    // Arrays only: their order is fixed, unlike a JSON object's keys, whose
    // order depends on how serde_json is built.
    let mut env: Vec<serde_json::Value> = server
        .env
        .iter()
        .map(|(name, entry)| match entry {
            McpEnvEntry::Plain(value) => serde_json::json!([name, "plain", value]),
            McpEnvEntry::Object(object) => serde_json::json!([
                name,
                "object",
                object.credential,
                object
                    .value
                    .as_ref()
                    .map(|value| hex(&Sha256::digest(value.secret().expose().as_bytes()))),
            ]),
        })
        .collect();
    env.sort_by_key(|entry| entry[0].as_str().unwrap_or_default().to_string());
    let canonical = serde_json::json!(["launch-v1", server.command, server.args, env]);
    format!(
        "sha256:{}",
        hex(&Sha256::digest(canonical.to_string().as_bytes()))
    )
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// `query` cut to at most `max_chars` characters and `max_words` words. A cut
/// ends at a word boundary, so no word is sent half.
#[must_use]
pub fn bound_query(query: &str, max_chars: usize, max_words: Option<usize>) -> String {
    let mut out = String::new();
    for (index, word) in query.split_whitespace().enumerate() {
        if max_words.is_some_and(|limit| index >= limit) {
            break;
        }
        let extra = usize::from(!out.is_empty()) + word.chars().count();
        if out.chars().count() + extra > max_chars {
            if out.is_empty() {
                // One word longer than the limit: cut it rather than send nothing.
                out = word.chars().take(max_chars).collect();
            }
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::schema::{McpEnvObject, SensitiveLiteral};

    fn entry() -> ManagedSearchBackend {
        let mut backend = ManagedSearchBackend {
            tier: SearchTier::Keyless,
            command: "uvx".to_string(),
            args: vec!["search-package==1.0.0".to_string()],
            env: IndexMap::new(),
            tool: "search".to_string(),
            launch_identity: String::new(),
            call_timeout_secs: 45,
            handshake_timeout_secs: 120,
            max_query_chars: 400,
            max_query_words: None,
            unattended: false,
        };
        backend.launch_identity = launch_identity(&backend.server());
        backend
    }

    #[test]
    fn a_recorded_identity_matches_until_the_launch_changes() {
        let mut backend = entry();
        assert!(backend.identity_matches());
        backend.args.push("--extra".to_string());
        assert!(!backend.identity_matches());
        let mut backend = entry();
        backend.command = "other".to_string();
        assert!(!backend.identity_matches());
        let mut backend = entry();
        backend
            .env
            .insert("LOG".to_string(), McpEnvEntry::Plain("debug".to_string()));
        assert!(!backend.identity_matches());
    }

    #[test]
    fn identity_ignores_env_order_and_never_holds_a_literal() {
        let mut a = McpServerConfig {
            command: "c".to_string(),
            ..McpServerConfig::default()
        };
        let mut b = a.clone();
        let literal = McpEnvEntry::Object(McpEnvObject {
            value: Some(SensitiveLiteral::new("hunter2-secret")),
            credential: None,
        });
        a.env
            .insert("A".to_string(), McpEnvEntry::Plain("1".to_string()));
        a.env.insert("KEY".to_string(), literal.clone());
        b.env.insert("KEY".to_string(), literal);
        b.env
            .insert("A".to_string(), McpEnvEntry::Plain("1".to_string()));
        assert_eq!(launch_identity(&a), launch_identity(&b));
        assert!(launch_identity(&a).starts_with("sha256:"));
        assert!(!launch_identity(&a).contains("hunter2"));
        let mut c = a.clone();
        c.env.insert(
            "KEY".to_string(),
            McpEnvEntry::Object(McpEnvObject {
                value: Some(SensitiveLiteral::new("other")),
                credential: None,
            }),
        );
        assert_ne!(launch_identity(&a), launch_identity(&c));
    }

    #[test]
    fn identity_is_pinned_for_a_known_launch() {
        // The recorded value must not change between builds or releases; a
        // change here invalidates every set-up backend.
        let server = McpServerConfig {
            command: "uvx".to_string(),
            args: vec!["pkg==1.0".to_string()],
            ..McpServerConfig::default()
        };
        let canonical = r#"["launch-v1","uvx",["pkg==1.0"],[]]"#;
        assert_eq!(
            launch_identity(&server),
            format!("sha256:{}", hex(&Sha256::digest(canonical.as_bytes())))
        );
    }

    #[test]
    fn queries_are_cut_at_word_boundaries() {
        assert_eq!(bound_query("one two three", 9, None), "one two");
        assert_eq!(bound_query("one two three", 400, Some(2)), "one two");
        assert_eq!(bound_query("  spaced   out  ", 400, None), "spaced out");
        assert_eq!(bound_query("abcdefghij", 4, None), "abcd");
        assert_eq!(bound_query("", 4, None), "");
    }
}
