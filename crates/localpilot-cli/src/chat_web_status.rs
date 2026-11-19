//! Chat web access as configured, read from the configuration alone (no server
//! is started and no search is sent here): shared by the chat web gateway and
//! `localpilot doctor`.

use std::path::{Path, PathBuf};

#[cfg(test)]
use localpilot_config::McpServerConfig;
use localpilot_config::{ChatWebMode, Config};

/// The egress audit log when `[research.web] audit_log` is unset.
pub(crate) fn default_audit_log(root: &Path) -> PathBuf {
    root.join(".localpilot")
        .join("research")
        .join("egress-audit.log")
}

/// Where chat web and research requests are recorded.
pub(crate) fn audit_log_path(root: &Path, config: &Config) -> PathBuf {
    config
        .research
        .web
        .audit_log
        .clone()
        .map_or_else(|| default_audit_log(root), |path| root.join(path))
}

/// Chat web access as configured, for `localpilot doctor`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ChatWebStatus {
    /// `[tools] web`: `off` or `ask`.
    pub mode: &'static str,
    /// `[research.web] enabled`, the switch chat web shares with /research.
    pub research_web_enabled: bool,
    /// Where each request is recorded.
    pub audit_log: String,
    /// Search tools `web_search` would use, as `server.tool`. Configured, not
    /// verified: no search is sent; doctor's MCP section probes the servers.
    pub search_backends: Vec<String>,
    /// Search tools named in `[research.mcp] tools` with no such server.
    pub unresolved: Vec<String>,
    /// Layers that tried to set `[search]` and were ignored: it is read from
    /// the user's own config file only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub search_ignored: Vec<&'static str>,
}

impl ChatWebStatus {
    pub fn from_config(root: &Path, config: &Config) -> Self {
        // Read from configuration alone; kept free of the launch code so the
        // doctor test build can include this file on its own.
        let mut search_backends = Vec::new();
        let mut unresolved = Vec::new();
        for pair in &config.research.mcp.tools {
            let label = format!("{}.{}", pair.server, pair.tool);
            if config.mcp.servers.contains_key(&pair.server) {
                search_backends.push(label);
            } else {
                unresolved.push(label);
            }
        }
        for (id, managed) in &config.search.managed {
            search_backends.push(format!(
                "{id}.{} (managed; starts on first search)",
                managed.tool
            ));
        }
        Self {
            mode: match config.tools.web {
                ChatWebMode::Off => "off",
                ChatWebMode::Ask => "ask",
            },
            research_web_enabled: config.research.web.enabled,
            audit_log: audit_log_path(root, config).display().to_string(),
            search_backends,
            unresolved,
            search_ignored: config.search_ignored.clone(),
        }
    }

    /// Whether chat may reach the web at all under this configuration.
    pub fn enabled(&self) -> bool {
        self.mode == "ask" && self.research_web_enabled
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn doctor_status_reads_the_configuration_without_starting_anything() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.mcp.servers.insert(
            "finder".to_string(),
            McpServerConfig {
                command: "finder".to_string(),
                ..McpServerConfig::default()
            },
        );
        for server in ["finder", "missing"] {
            config
                .research
                .mcp
                .tools
                .push(localpilot_config::ResearchMcpTool {
                    server: server.to_string(),
                    tool: "search".to_string(),
                });
        }
        let status = ChatWebStatus::from_config(dir.path(), &config);
        assert_eq!(status.mode, "ask");
        assert_eq!(status.search_backends, vec!["finder.search"]);
        assert_eq!(status.unresolved, vec!["missing.search"]);
        assert_eq!(
            status.audit_log,
            default_audit_log(dir.path()).display().to_string()
        );
        assert_eq!(status.enabled(), config.research.web.enabled);

        config.tools.web = ChatWebMode::Off;
        let off = ChatWebStatus::from_config(dir.path(), &config);
        assert_eq!(off.mode, "off");
        assert!(!off.enabled());
    }
}
