//! Import a Claude Code session into LocalPilot's session store.
//!
//! Claude Code stores each session as a JSONL file under
//! `~/.claude/projects/<encoded-cwd>/<sessionId>.jsonl`, one JSON object per
//! line, with one message content block per line chained by `uuid`/`parentUuid`.
//! We read that file only as a behaviour reference (clean-room); the import,
//! mapping, and markers here are original.
//!
//! Foreign tool calls carry ids and schemas that a *different* provider would
//! reject on resume, and reasoning blocks carry provider-specific signatures that
//! cannot be replayed. So the import **text-flattens** the history — tool calls
//! and results become plain text markers, reasoning is dropped — leaving prose
//! that any provider serializes verbatim. The result is a first-class LocalPilot
//! session resumable by the name `imported_cc_<sessionId>`.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use localpilot_core::{ContentBlock, Message, Role, SessionId};
use localpilot_store::{origin_for, OpenReason, SessionEventKind, Store};
use serde::Deserialize;

/// A parsed Claude Code log line. Lenient: only the fields the import needs, and
/// a line that fails to parse is skipped rather than aborting the import.
#[derive(Debug, Deserialize)]
struct CcLine {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<CcMessage>,
    #[serde(default, rename = "isSidechain")]
    is_sidechain: bool,
}

#[derive(Debug, Deserialize)]
struct CcMessage {
    content: CcContent,
}

/// A user message is a plain string; a tool-result user turn and every assistant
/// turn are arrays of typed blocks.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CcContent {
    Text(String),
    Blocks(Vec<CcBlock>),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum CcBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        name: String,
        #[serde(default)]
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        #[serde(default)]
        content: serde_json::Value,
    },
    // `thinking` and any other block type are ignored (unreplayable / metadata).
    #[serde(other)]
    Other,
}

/// Bound on a flattened tool marker, so one giant tool input/result does not
/// bloat the imported transcript.
const MARKER_CHAR_CAP: usize = 500;

fn bounded(text: &str) -> String {
    if text.chars().count() <= MARKER_CHAR_CAP {
        return text.to_string();
    }
    let cut: String = text.chars().take(MARKER_CHAR_CAP).collect();
    format!("{cut}…")
}

/// A tool_result's `content` (string or array of `{text}` parts) as plain text.
fn flatten_result(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| p.as_str().map(str::to_string))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Map one Claude Code line to a text-flattened LocalPilot message, or `None`
/// when the line is metadata, a sidechain, reasoning-only, or empty.
fn map_cc_line(line: &CcLine) -> Option<Message> {
    if line.is_sidechain {
        return None;
    }
    let content = &line.message.as_ref()?.content;
    match line.kind.as_str() {
        "user" => match content {
            CcContent::Text(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| Message::text(Role::User, text))
            }
            CcContent::Blocks(blocks) => {
                let parts: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| match b {
                        CcBlock::Text { text } => Some(text.clone()),
                        CcBlock::ToolResult { content } => {
                            Some(format!("[tool result] {}", bounded(&flatten_result(content))))
                        }
                        _ => None,
                    })
                    .collect();
                (!parts.is_empty())
                    .then(|| Message::new(Role::User, vec![ContentBlock::text(parts.join("\n"))]))
            }
        },
        "assistant" => {
            let parts: Vec<String> = match content {
                CcContent::Text(text) => vec![text.clone()],
                CcContent::Blocks(blocks) => blocks
                    .iter()
                    .filter_map(|b| match b {
                        CcBlock::Text { text } => Some(text.clone()),
                        CcBlock::ToolUse { name, input } => {
                            Some(format!("[tool: {name}({})]", bounded(&input.to_string())))
                        }
                        // reasoning (`thinking`) and unknown blocks are dropped.
                        _ => None,
                    })
                    .collect(),
            };
            (!parts.is_empty())
                .then(|| Message::new(Role::Assistant, vec![ContentBlock::text(parts.join("\n"))]))
        }
        _ => None,
    }
}

/// Encode a working directory the way Claude Code names its project folder:
/// every path separator (`:`, `\`, `/`) becomes a single `-`.
fn encode_cwd(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if matches!(c, ':' | '\\' | '/') { '-' } else { c })
        .collect()
}

/// Resolve the `.jsonl` file to import: an explicit file, a project directory
/// (newest session, or `session`), or the encoded current directory.
fn locate_jsonl(cwd: &Path, project: Option<&Path>, session: Option<&str>) -> Result<PathBuf> {
    let claude_projects = dirs_home()?.join(".claude").join("projects");
    let dir = match project {
        Some(p) if p.extension().is_some_and(|e| e == "jsonl") => return Ok(p.to_path_buf()),
        Some(p) if p.is_dir() => p.to_path_buf(),
        Some(p) => claude_projects.join(encode_cwd(p)),
        None => claude_projects.join(encode_cwd(cwd)),
    };
    if let Some(id) = session {
        let candidate = dir.join(format!("{id}.jsonl"));
        if candidate.is_file() {
            return Ok(candidate);
        }
        bail!("no Claude Code session {id}.jsonl under {}", dir.display());
    }
    // Newest .jsonl in the project dir.
    let newest = std::fs::read_dir(&dir)
        .with_context(|| format!("no Claude Code project dir {}", dir.display()))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        });
    newest.with_context(|| format!("no Claude Code session .jsonl under {}", dir.display()))
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("could not resolve the home directory")
}

/// Import the Claude Code session at `jsonl_path` into `store` under the name
/// `imported_cc_<stem>`. Refuses to steal the name from an existing session
/// unless `force` (which imports under a suffixed name); never overwrites.
pub fn import_session(
    store: &Store,
    jsonl_path: &Path,
    force: bool,
    out: &mut dyn Write,
) -> Result<SessionId> {
    let cc_id = jsonl_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let base_name = format!("imported_cc_{cc_id}");
    let name = match store.find_session_by_name(&base_name)? {
        None => base_name,
        Some(_) if force => format!("{base_name}_{}", now_suffix()),
        Some(_) => bail!(
            "{base_name} was already imported; re-import under a new name with --force \
             (a local continuation of it is never overwritten)"
        ),
    };

    let raw = std::fs::read_to_string(jsonl_path)
        .with_context(|| format!("reading {}", jsonl_path.display()))?;
    let messages: Vec<Message> = raw
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CcLine>(l).ok())
        .filter_map(|line| map_cc_line(&line))
        .collect();
    if messages.is_empty() {
        bail!("no importable messages found in {}", jsonl_path.display());
    }

    let session = SessionId::new();
    let mut parent = store.append_event(session, None, SessionEventKind::SessionOpened {
        reason: OpenReason::New,
    })?;
    for message in &messages {
        // Write both: the transcript (drives the session-list message count) and
        // the event log (what resume reads back). Redaction is applied on write.
        store.append_message(session, message)?;
        parent = store.append_event(
            session,
            Some(parent),
            SessionEventKind::Message {
                message: message.clone(),
                origin: origin_for(message),
            },
        )?;
    }
    store.append_event(session, Some(parent), SessionEventKind::SessionClosed)?;
    store.set_session_name(session, &name)?;

    writeln!(
        out,
        "imported {} messages from {} as {name}\nresume it with: localpilot --resume {name}",
        messages.len(),
        jsonl_path.display()
    )?;
    Ok(session)
}

/// Top-level `import claude-code` entry: resolve the file then import it.
pub fn import_claude_code(
    store: &Store,
    cwd: &Path,
    project: Option<&Path>,
    session: Option<&str>,
    force: bool,
    out: &mut dyn Write,
) -> Result<SessionId> {
    let jsonl = locate_jsonl(cwd, project, session)?;
    import_session(store, &jsonl, force, out)
}

/// A short deterministic-enough suffix for a forced re-import name. Uses the file
/// path's length + a store-provided uniqueness fallback would be better, but a
/// monotonic-ish suffix from the wall clock is fine for a manual re-import.
fn now_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"type":"summary","summary":"skip me"}
{"type":"user","isSidechain":false,"message":{"role":"user","content":"read the file"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret reasoning","signature":"CAIS"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"reading now"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"a.rs"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"file body here"}]}}
{"type":"mode","mode":"default"}
{"type":"user","isSidechain":true,"message":{"role":"user","content":"sidechain — skip"}}"#;

    fn mapped(fixture: &str) -> Vec<Message> {
        fixture
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str::<CcLine>(l).ok())
            .filter_map(|line| map_cc_line(&line))
            .collect()
    }

    #[test]
    fn flattens_and_drops_the_right_lines() {
        let messages = mapped(FIXTURE);
        // summary/mode metadata skipped; thinking dropped; sidechain skipped.
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(
            messages.iter().map(|m| m.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant, Role::Assistant, Role::User]
        );
        // No structured tool/reasoning blocks survive — everything is text.
        for m in &messages {
            for b in &m.content {
                assert!(matches!(b, ContentBlock::Text { .. }), "not flattened: {b:?}");
            }
        }
        let text = |m: &Message| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .collect::<String>()
        };
        assert_eq!(text(&messages[0]), "read the file");
        assert_eq!(text(&messages[1]), "reading now");
        assert!(text(&messages[2]).starts_with("[tool: Read("), "{}", text(&messages[2]));
        assert!(text(&messages[3]).starts_with("[tool result] "), "{}", text(&messages[3]));
        assert!(!text(&messages[2]).contains("secret reasoning"));
    }

    #[test]
    fn encode_cwd_matches_claude_codes_folder_scheme() {
        assert_eq!(
            encode_cwd(Path::new(r"D:\repos\LocalX\LocalPilot")),
            "D--repos-LocalX-LocalPilot"
        );
    }

    #[test]
    fn import_round_trips_into_a_resumable_named_session() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path());
        let jsonl = dir.path().join("abc123.jsonl");
        std::fs::write(&jsonl, FIXTURE).unwrap();

        let mut out = Vec::new();
        let session = import_session(&store, &jsonl, false, &mut out).unwrap();

        // Resolvable by the resume name, and the event log rebuilds the flattened
        // four-message transcript.
        assert_eq!(
            store
                .find_session_by_name("imported_cc_abc123")
                .unwrap()
                .map(|e| e.id),
            Some(session)
        );
        let events = store.read_events(session).unwrap();
        let transcript = localpilot_store::transcript_from_events(&events);
        assert_eq!(transcript.len(), 4);
        // The session-list count comes from the transcript writes, not zero.
        let listed = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|e| e.id == session)
            .unwrap();
        assert_eq!(listed.message_count, 4);

        // A second import of the same session is refused without --force, and
        // succeeds under a new name with it (never overwriting the first).
        assert!(import_session(&store, &jsonl, false, &mut Vec::new()).is_err());
        assert!(import_session(&store, &jsonl, true, &mut Vec::new()).is_ok());
    }
}
