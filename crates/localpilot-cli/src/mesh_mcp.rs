//! `localpilot mesh mcp --role <r>`: the pair mailbox as MCP tools, for a
//! host (Claude Code, Codex) that would rather call tools than run commands.
//!
//! MCP over stdio: JSON-RPC 2.0, one JSON object per line. The role is fixed
//! when the server starts and is never a tool argument, so a client can only
//! ever act as that participant. Each tool is one call to the same native
//! operation `localpilot mesh <op>` runs, and returns what that command would
//! print. There is no tool that waits for mail: waiting stays with
//! `localpilot mesh wait` (or `watch`) in a background task, so a tool call
//! never holds a host's turn open.

use std::process::ExitCode;

use clap::Parser;
use localpilot_mesh::ops::engine::{validate, Answer, Finding};
use localpilot_mesh::ops::{EvidenceArgs, PostArgs, WatchArgs};
use localpilot_mesh::{Mesh, MeshError, Out};
use localpilot_rpc::JsonRecordReader;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::mesh_cmd::{native_writer, resolve_anchor, MeshArgs};

/// The protocol revisions this server answers with, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26"];

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh mcp", no_binary_name = true)]
struct McpCli {
    /// The participant this server acts as, for every tool call.
    #[arg(long)]
    role: String,
}

/// Whether these mesh arguments ask for the MCP server.
pub(crate) fn is_mcp(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "mcp")
}

/// Serve MCP on stdio until the client closes it.
pub(crate) async fn run(args: MeshArgs) -> ExitCode {
    let cli = match McpCli::try_parse_from(&args.rest[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let (anchor, source) = match resolve_anchor(args.repo.as_deref()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };
    match native_writer() {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("localpilot mesh mcp needs the native writer; it does not run under [mesh] writer = \"delegate\"");
            return ExitCode::from(2);
        }
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    }
    let mesh = Mesh::at(&anchor, source);
    let mut reader = JsonRecordReader::new(tokio::io::stdin());
    let mut out = tokio::io::stdout();
    loop {
        let message = match reader.next().await {
            Ok(Some(m)) => m,
            Ok(None) => return ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("localpilot mesh mcp: {e}");
                return ExitCode::from(1);
            }
        };
        let Some(reply) = handle(&mesh, &cli.role, &message).await else {
            continue; // a notification
        };
        let mut line = reply.to_string().into_bytes();
        line.push(b'\n');
        if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
            return ExitCode::from(1);
        }
    }
}

/// The response to one JSON-RPC message, or `None` for a notification.
async fn handle(mesh: &Mesh, role: &str, message: &Value) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message["method"].as_str().unwrap_or_default();
    let result = match method {
        "initialize" => Ok(initialize(&message["params"])),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": catalog() })),
        "tools/call" => {
            let name = message["params"]["name"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let args = message["params"]["arguments"].clone();
            let result = call(mesh, role, &name, &args).await;
            // What the tool appended is pushed once it holds no lock (P-3).
            crate::mesh_push::push_all(mesh).await;
            Ok(result)
        }
        _ => Err((-32601, format!("method not found: {method}"))),
    };
    Some(match result {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err((code, msg)) => {
            json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
        }
    })
}

/// The client's protocol revision when this server speaks it, else the
/// newest this server speaks; the client decides whether to continue.
fn initialize(params: &Value) -> Value {
    let asked = params["protocolVersion"].as_str().unwrap_or_default();
    let version = PROTOCOL_VERSIONS
        .iter()
        .find(|v| **v == asked)
        .unwrap_or(&PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "localpilot-mesh", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn catalog() -> Vec<Value> {
    let string = |d: &str| json!({"type": "string", "description": d});
    let anchor_schema = json!({
        "type": "object",
        "description": "Lines pinned to their hash, exactly as `evidence` returned them.",
        "properties": {
            "path": {"type": "string"},
            "start": {"type": "integer"},
            "end": {"type": "integer"},
            "sha": {"type": "string"},
        },
        "required": ["path", "start", "end", "sha"],
    });
    vec![
        json!({"name": "status",
               "description": "The pair session's status, as `status` prints it.",
               "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "peek",
               "description": "Look once for mail and return it, as `peek` prints it; never waits. Under acknowledged delivery this presents the mail (it counts as delivered) but does not acknowledge it: it is shown again, marked REDELIVERED, until `ack` or a post's `ack_through` acknowledges it.",
               "inputSchema": {"type": "object", "properties": {}}}),
        json!({"name": "ack",
               "description": "Acknowledge mail through a point: N, or sender:N[,sender:N] in a three-party session.",
               "inputSchema": {"type": "object", "properties": {"through": string("The point to acknowledge through.")}, "required": ["through"]}}),
        json!({"name": "post",
               "description": "Post a message. Use `verdict` for a VERDICT.",
               "inputSchema": {"type": "object", "properties": {
                   "kind": string("PLAN, QUESTION, ANSWER, NOTE, REVIEW_REQUEST, ... (not VERDICT)."),
                   "body": string("The message body."),
                   "to": string("Recipients, comma-separated (three-party sessions)."),
                   "reply_to": string("The msg_id this answers (three-party sessions)."),
                   "expect_reply": {"type": "boolean", "description": "Whether a reply is expected."},
                   "ack_through": string("Acknowledge through this point first."),
               }, "required": ["kind", "body"]}}),
        json!({"name": "handoff",
               "description": "Offer the unit's ownership, or accept an offer made to you.",
               "inputSchema": {"type": "object", "properties": {
                   "action": {"type": "string", "enum": ["offer", "accept"]},
                   "to": string("For an offer in a three-party session: to whom."),
               }, "required": ["action"]}}),
        json!({"name": "verdict",
               "description": "Answer the open REVIEW_REQUEST with a verdict. The header (decision, round, blocking and important counts) is written from your findings. Refused unless `reply_to` is the owner's latest request in this unit, you are a required reviewer who has not answered it yet, and its fingerprint manifest still holds.",
               "inputSchema": {"type": "object", "properties": {
                   "decision": {"type": "string", "enum": ["AGREE", "REVISE"]},
                   "reply_to": string("The REVIEW_REQUEST's msg_id."),
                   "findings": {"type": "array", "items": {"type": "object", "properties": {
                       "file": {"type": "string"},
                       "line": {"type": "integer"},
                       "severity": {"type": "string", "enum": ["blocking", "important", "minor"]},
                       "text": {"type": "string"},
                       "anchor": anchor_schema.clone(),
                   }, "required": ["file", "severity", "text"]}},
                   "body": string("Your summary."),
               }, "required": ["decision", "reply_to", "body"]}}),
        json!({"name": "evidence",
               "description": "Read-only evidence about the shared tree, as one JSON packet naming the session, unit and HEAD. `locate` finds text (hits come back as anchors); `anchor` pins lines start..end of a file to their hash; `verify` checks anchors against the files now (ok, moved, ambiguous, stale, unknown); `diagnostics` runs fixed Git reads and says whether each path exists. It never writes and runs no build or test command. To cite lines in a verdict, put an anchor from here in the finding.",
               "inputSchema": {"type": "object", "properties": {
                   "op": {"type": "string", "enum": ["locate", "anchor", "verify", "diagnostics"]},
                   "query": string("locate: the text, or a regular expression with `regex`."),
                   "regex": {"type": "boolean", "description": "locate: read `query` as a regular expression."},
                   "glob": string("locate: only paths matching this glob."),
                   "path": string("anchor: the file, relative to the tree."),
                   "start": {"type": "integer", "description": "anchor: the first line, from 1."},
                   "end": {"type": "integer", "description": "anchor: the last line, inclusive."},
                   "anchors": {"type": "array", "items": anchor_schema, "description": "verify: the anchors to check."},
                   "paths": {"type": "array", "items": {"type": "string"}, "description": "diagnostics: paths to check exist."},
               }, "required": ["op"]}}),
    ]
}

/// The evidence service as a tool: the packet, or why it was refused.
fn evidence(mesh: &Mesh, role: &str, args: &Value) -> Result<Value, MeshError> {
    let usize_arg = |k: &str| {
        args.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
    };
    let request = match args["op"].as_str() {
        Some("locate") => match text_arg(args, "query") {
            Some(query) => EvidenceArgs::Locate {
                query,
                regex: args["regex"].as_bool().unwrap_or(false),
                glob: text_arg(args, "glob"),
            },
            None => return Ok(tool_error("locate needs `query`")),
        },
        Some("anchor") => match (text_arg(args, "path"), usize_arg("start"), usize_arg("end")) {
            (Some(path), Some(start), Some(end)) => EvidenceArgs::Anchor { path, start, end },
            _ => return Ok(tool_error("anchor needs `path`, `start` and `end`")),
        },
        Some("verify") => {
            match serde_json::from_value(args.get("anchors").cloned().unwrap_or(Value::Null)) {
                Ok(anchors) => EvidenceArgs::Verify { anchors },
                Err(e) => return Ok(tool_error(&format!("anchors: {e}"))),
            }
        }
        Some("diagnostics") => EvidenceArgs::Diagnostics {
            paths: args["paths"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        },
        _ => {
            return Ok(tool_error(
                "evidence needs `op`: locate, anchor, verify or diagnostics",
            ))
        }
    };
    Ok(match mesh.evidence(role, &request) {
        Ok(packet) => json!({
            "content": [{"type": "text", "text": serde_json::to_string_pretty(&packet).unwrap_or_default()}],
            "structuredContent": packet,
            "isError": false,
        }),
        Err(MeshError::Refused(why)) => tool_error(&why),
        Err(e) => return Err(e),
    })
}

/// A tool result carrying a command's output.
fn from_out(out: &Out) -> Value {
    let text = format!("{}{}", out.stdout, out.stderr);
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": {"code": out.code, "stdout": out.stdout, "stderr": out.stderr},
        "isError": out.code != 0,
    })
}

fn tool_error(message: &str) -> Value {
    json!({"content": [{"type": "text", "text": message}], "isError": true})
}

fn text_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_owned)
}

async fn call(mesh: &Mesh, role: &str, name: &str, args: &Value) -> Value {
    let (mesh, role, name, args) = (mesh.clone(), role.to_owned(), name.to_owned(), args.clone());
    tokio::task::spawn_blocking(move || match run_tool(&mesh, &role, &name, &args) {
        Ok(v) => v,
        Err(e) => tool_error(&e.to_string()),
    })
    .await
    .unwrap_or_else(|e| tool_error(&e.to_string()))
}

fn run_tool(mesh: &Mesh, role: &str, name: &str, args: &Value) -> Result<Value, MeshError> {
    Ok(match name {
        "status" => from_out(&mesh.status()?),
        "peek" => {
            let mut text = String::new();
            let a = WatchArgs {
                block: false,
                ..WatchArgs::default()
            };
            let code = mesh.watch(role, &a, &mut |t| {
                text.push_str(t);
                text.push('\n');
                Ok(())
            })?;
            from_out(&Out {
                code,
                stdout: text,
                stderr: String::new(),
            })
        }
        "ack" => match text_arg(args, "through") {
            Some(t) => from_out(&mesh.ack(role, &t)?),
            None => tool_error("ack needs `through`"),
        },
        "post" => {
            let (Some(kind), Some(body)) = (text_arg(args, "kind"), text_arg(args, "body")) else {
                return Ok(tool_error("post needs `kind` and `body`"));
            };
            if kind == "VERDICT" {
                return Ok(tool_error("use the `verdict` tool for a VERDICT"));
            }
            let a = PostArgs {
                kind,
                body,
                expect_reply: args["expect_reply"].as_bool().unwrap_or(false),
                to: text_arg(args, "to"),
                reply_to: text_arg(args, "reply_to"),
                ack_through: text_arg(args, "ack_through"),
                ..PostArgs::default()
            };
            from_out(&mesh.post(role, &a)?)
        }
        "handoff" => match args["action"].as_str() {
            Some("offer") => from_out(&mesh.handoff_offer(role, text_arg(args, "to").as_deref())?),
            Some("accept") => from_out(&mesh.handoff_accept(role)?),
            _ => tool_error("handoff needs action `offer` or `accept`"),
        },
        "verdict" => verdict(mesh, role, args)?,
        "evidence" => evidence(mesh, role, args)?,
        other => tool_error(&format!("no tool named {other}")),
    })
}

fn verdict(mesh: &Mesh, role: &str, args: &Value) -> Result<Value, MeshError> {
    let (Some(decision), Some(reply_to), Some(body)) = (
        text_arg(args, "decision"),
        text_arg(args, "reply_to"),
        text_arg(args, "body"),
    ) else {
        return Ok(tool_error(
            "verdict needs `decision`, `reply_to` and `body`",
        ));
    };
    let findings: Vec<Finding> =
        match serde_json::from_value(args.get("findings").cloned().unwrap_or_else(|| json!([]))) {
            Ok(f) => f,
            Err(e) => return Ok(tool_error(&format!("findings: {e}"))),
        };
    let answer = Answer {
        kind: "VERDICT".into(),
        decision: Some(decision),
        findings,
        body,
    };
    let request = match mesh.open_review(role, &reply_to)? {
        Ok(r) => r,
        Err(why) => return Ok(tool_error(&why)),
    };
    let post = match validate(mesh.root(), &request, &answer) {
        Ok(p) => p,
        Err(why) => return Ok(tool_error(&why)),
    };
    // Checked again just before the post: a newer request or a changed tree
    // since the first check refuses rather than posts. (Nothing locks the
    // working tree between this check and the post; the window is this
    // function's own run time.)
    let request = match mesh.open_review(role, &reply_to)? {
        Ok(r) => r,
        Err(why) => return Ok(tool_error(&why)),
    };
    let m = mesh.post_verdict(role, &request, post)?;
    let id = m.get("msg_id").and_then(Value::as_str).map_or_else(
        || {
            format!(
                "{role}:{}",
                m.get("seq").and_then(Value::as_i64).unwrap_or(0)
            )
        },
        str::to_owned,
    );
    let header = m
        .get("body")
        .and_then(Value::as_str)
        .and_then(|b| b.lines().next())
        .unwrap_or_default()
        .to_owned();
    Ok(from_out(&Out {
        code: 0,
        stdout: format!("POSTED VERDICT {id} reply_to={reply_to}\n{header}\n"),
        stderr: String::new(),
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_known_protocol_revision_is_echoed_and_an_unknown_one_gets_the_newest() {
        assert_eq!(
            initialize(&json!({"protocolVersion": "2025-03-26"}))["protocolVersion"],
            "2025-03-26"
        );
        assert_eq!(
            initialize(&json!({"protocolVersion": "1999-01-01"}))["protocolVersion"],
            "2025-06-18"
        );
        assert_eq!(initialize(&json!({}))["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn the_catalog_has_the_seven_tools_no_wait_and_no_role_argument() {
        let tools = catalog();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["status", "peek", "ack", "post", "handoff", "verdict", "evidence"]
        );
        let all = serde_json::to_string(&tools).unwrap();
        assert!(!all.contains("\"role\""), "a role argument appeared");
        assert!(!all.contains("\"wait"), "a wait tool or argument appeared");
    }
}
