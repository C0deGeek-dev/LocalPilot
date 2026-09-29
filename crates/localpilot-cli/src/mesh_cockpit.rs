//! `localpilot mesh cockpit`: the human's view of a pair session.
//!
//! The cockpit is an observer. It has no role, registers no delivery
//! endpoint and writes nothing: it re-reads a typed snapshot of the session
//! (`Mesh::snapshot`). `--json` prints one snapshot and exits.

use std::process::ExitCode;

use clap::Parser;
use localpilot_mesh::Mesh;

use crate::mesh_cmd::{resolve_anchor, MeshArgs};

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh cockpit", no_binary_name = true)]
struct CockpitCli {
    /// Print one snapshot of the session as JSON and exit.
    #[arg(long)]
    json: bool,
}

/// Whether these mesh arguments ask for the cockpit.
pub(crate) fn is_cockpit(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "cockpit")
}

/// Show the cockpit; 0 on a clean exit, 1 on an error, 2 on a usage error.
pub(crate) fn run(args: &MeshArgs) -> ExitCode {
    let cli = match CockpitCli::try_parse_from(&args.rest[1..]) {
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
    let mesh = Mesh::at(&anchor, source);
    if cli.json {
        let snap = mesh.snapshot();
        println!(
            "{}",
            serde_json::to_string_pretty(&snap).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    eprintln!("localpilot mesh cockpit: the full-screen view is not built yet; use --json");
    ExitCode::from(2)
}
