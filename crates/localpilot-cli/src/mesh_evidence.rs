//! `localpilot mesh evidence`: the read-only evidence service on the command
//! line, for a participant of the active pair session.
//!
//! ```text
//! localpilot mesh evidence --role <r> locate --query <text> [--regex] [--glob <g>]
//! localpilot mesh evidence --role <r> anchor --path <p> --start <n> --end <m>
//! localpilot mesh evidence --role <r> verify --anchors '<json array>'
//! localpilot mesh evidence --role <r> diagnostics [--path <p>]...
//! ```
//!
//! Each prints one JSON packet on stdout and exits 0; a refused request exits
//! 1 with the reason on stderr. It reads the tree and the session record and
//! writes nothing, so it runs under either mesh writer.

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use localpilot_mesh::evidence::Anchor;
use localpilot_mesh::ops::EvidenceArgs;
use localpilot_mesh::Mesh;

use crate::mesh_cmd::{resolve_anchor, MeshArgs};

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh evidence", no_binary_name = true)]
struct EvidenceCli {
    /// The participant asking.
    #[arg(long)]
    role: String,
    #[command(subcommand)]
    op: EvidenceOp,
}

#[derive(Debug, Subcommand)]
enum EvidenceOp {
    /// Find text in the tree.
    Locate {
        #[arg(long)]
        query: String,
        /// Read the query as a regular expression.
        #[arg(long)]
        regex: bool,
        /// Only files whose path matches this glob (`.pairignore` grammar).
        #[arg(long)]
        glob: Option<String>,
    },
    /// Pin lines `start..=end` of a file to their hash.
    Anchor {
        #[arg(long)]
        path: String,
        #[arg(long)]
        start: usize,
        #[arg(long)]
        end: usize,
    },
    /// Check anchors (a JSON array of `{path, start, end, sha}`).
    Verify {
        #[arg(long)]
        anchors: String,
    },
    /// The fixed Git reads, and whether each `--path` exists.
    Diagnostics {
        #[arg(long = "path")]
        paths: Vec<String>,
    },
}

/// Whether these mesh arguments ask for the evidence service.
pub(crate) fn is_evidence(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "evidence")
}

/// Answer one evidence request.
pub(crate) fn run(args: &MeshArgs) -> ExitCode {
    let cli = match EvidenceCli::try_parse_from(&args.rest[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let request = match request_of(cli.op) {
        Ok(r) => r,
        Err(msg) => {
            eprintln!("localpilot mesh evidence: {msg}");
            return ExitCode::from(2);
        }
    };
    let (anchor, source) = match resolve_anchor(args.repo.as_deref()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };
    match Mesh::at(&anchor, source).evidence(&cli.role, &request) {
        Ok(packet) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&packet).unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e} [anchor={} source={source}]", anchor.display());
            ExitCode::from(1)
        }
    }
}

fn request_of(op: EvidenceOp) -> Result<EvidenceArgs, String> {
    Ok(match op {
        EvidenceOp::Locate { query, regex, glob } => EvidenceArgs::Locate { query, regex, glob },
        EvidenceOp::Anchor { path, start, end } => EvidenceArgs::Anchor { path, start, end },
        EvidenceOp::Verify { anchors } => EvidenceArgs::Verify {
            anchors: serde_json::from_str::<Vec<Anchor>>(&anchors)
                .map_err(|e| format!("--anchors: {e}"))?,
        },
        EvidenceOp::Diagnostics { paths } => EvidenceArgs::Diagnostics { paths },
    })
}
