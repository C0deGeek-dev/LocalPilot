//! Managed search backends as `localpilot doctor` reports them: what is
//! configured, and what can be read without starting anything. The caller
//! collects it; nothing here starts a search bridge or a container, or sends a
//! query.

use std::fmt::Write as _;

/// One `[search.managed.<id>]` entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SearchReadiness {
    pub id: String,
    pub tier: String,
    pub tool: String,
    /// Its place among the search candidates web search and /research try,
    /// 1 being first; `None` when web research is off. A place, not a
    /// promise that it starts.
    pub order: Option<usize>,
    /// Chat's `web_search` may use it too (`[tools] web` is on as well);
    /// otherwise only /research does.
    pub chat_web_search: bool,
    /// The recorded launch identity still matches what the entry runs.
    pub identity_valid: bool,
    /// The command the entry runs exists.
    pub command_present: bool,
    pub unattended: bool,
    /// The local SearXNG LocalPilot set up for this entry, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine: Option<EngineReadiness>,
}

/// The local SearXNG behind a managed entry, as recorded and as seen.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EngineReadiness {
    /// `recorded`, `missing` (no LocalPilot record) or `unreadable`.
    pub record: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The following are unknown (`None`) without a readable record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_current: Option<bool>,
    /// The config table is exactly the one LocalPilot recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table_matches_record: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_unfinished: Option<bool>,
    /// Running, stopped, or why it was not checked (fixed wording, no
    /// command output).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    /// Whether it answers on 127.0.0.1; only asked when it runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answering: Option<bool>,
}

fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (1, 11) | (2, 12) | (3, 13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The doctor lines for the managed search backends; nothing when there are none.
pub fn render(s: &mut String, backends: &[SearchReadiness]) {
    if backends.is_empty() {
        return;
    }
    let _ = writeln!(
        s,
        "  managed search backends (configured; doctor starts no bridge and sends no query):"
    );
    for backend in backends {
        let order = backend.order.map_or_else(
            || "not a search candidate while web research is off".to_string(),
            |n| {
                format!(
                    "search candidate {} for {}",
                    ordinal(n),
                    if backend.chat_web_search {
                        "/research and chat web_search"
                    } else {
                        "/research only (chat web access is off)"
                    }
                )
            },
        );
        let _ = writeln!(
            s,
            "    {} ({}, tool {}): {order}; launch identity {}; command {}; unattended use {}",
            backend.id,
            backend.tier,
            backend.tool,
            if backend.identity_valid {
                "valid"
            } else {
                "changed, so it will not start"
            },
            if backend.command_present {
                "present"
            } else {
                "missing"
            },
            if backend.unattended { "allowed" } else { "off" },
        );
        if let Some(engine) = &backend.engine {
            render_engine(s, engine);
        }
    }
}

fn render_engine(s: &mut String, engine: &EngineReadiness) {
    match engine.record {
        "missing" => {
            let _ = writeln!(
                s,
                "      SearXNG: LocalPilot has no record of it (set up by hand, or removed); doctor does not check it"
            );
            return;
        }
        "unreadable" => {
            let _ = writeln!(
                s,
                "      SearXNG: LocalPilot's record of it could not be read; doctor does not check it"
            );
            return;
        }
        _ => {}
    }
    let address = engine.port.map_or_else(
        || "SearXNG".to_string(),
        |port| format!("SearXNG 127.0.0.1:{port}"),
    );
    let answering = match engine.answering {
        Some(true) => ", answering",
        Some(false) => ", not answering",
        None => "",
    };
    let image = match engine.image_current {
        Some(true) => "current",
        Some(false) => "older (localpilot search update)",
        None => "unknown",
    };
    let table = match engine.table_matches_record {
        Some(true) => "as LocalPilot wrote it",
        Some(false) => "not the one LocalPilot recorded",
        None => "unknown",
    };
    let _ = writeln!(
        s,
        "      {address}: container {}{answering}; image {image}; config table {table}{}",
        engine.container.as_deref().unwrap_or("not checked"),
        if engine.update_unfinished == Some(true) {
            "; an update did not finish (localpilot search update)"
        } else {
            ""
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_each_fact_separately_and_nothing_when_empty() {
        let mut empty = String::new();
        render(&mut empty, &[]);
        assert!(empty.is_empty());
        let mut s = String::new();
        render(
            &mut s,
            &[SearchReadiness {
                id: "searxng".to_string(),
                tier: "self_hosted".to_string(),
                tool: "searxng_web_search".to_string(),
                order: Some(2),
                chat_web_search: true,
                identity_valid: true,
                command_present: false,
                unattended: false,
                engine: Some(EngineReadiness {
                    record: "recorded",
                    port: Some(8888),
                    image_current: Some(true),
                    table_matches_record: Some(true),
                    update_unfinished: Some(false),
                    container: Some("running".to_string()),
                    answering: Some(true),
                }),
            }],
        );
        let mut off = String::new();
        render(
            &mut off,
            &[SearchReadiness {
                id: "searxng".to_string(),
                tier: "self_hosted".to_string(),
                tool: "searxng_web_search".to_string(),
                order: None,
                chat_web_search: false,
                identity_valid: true,
                command_present: true,
                unattended: false,
                engine: Some(EngineReadiness {
                    record: "missing",
                    port: None,
                    image_current: None,
                    table_matches_record: None,
                    update_unfinished: None,
                    container: None,
                    answering: None,
                }),
            }],
        );
        assert!(
            off.contains("not a search candidate while web research is off"),
            "{off}"
        );
        assert!(off.contains("LocalPilot has no record of it"), "{off}");
        assert!(!off.contains("older"), "{off}");
        for needed in [
            "doctor starts no bridge and sends no query",
            "search candidate 2nd for /research and chat web_search",
            "launch identity valid",
            "command missing",
            "SearXNG 127.0.0.1:8888: container running, answering",
            "config table as LocalPilot wrote it",
        ] {
            assert!(s.contains(needed), "{needed}: {s}");
        }
        assert_eq!(ordinal(11), "11th");
        assert_eq!(ordinal(23), "23rd");
    }
}
