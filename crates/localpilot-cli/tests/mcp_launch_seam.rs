//! Every MCP launch path goes through one resolver and one spawn.
//!
//! Session server discovery, designated research search, and `doctor` each used
//! to call `StdioTransport::spawn` themselves. Three copies of a launch policy
//! is how they drift: a change to what an entry form means, or to what a missing
//! credential does, lands in one and silently misses the others — which is the
//! defect this seam exists to prevent, not merely a tidiness preference.
//!
//! Source-text checks, because the alternative is standing up three live MCP
//! servers to prove a structural property.

const MCP_SRC: &str = include_str!("../src/mcp.rs");
const RESEARCH_SRC: &str = include_str!("../src/research.rs");
const DOCTOR_SRC: &str = include_str!("../src/doctor.rs");
const SEAM_SRC: &str = include_str!("../src/mcp_env.rs");

/// The launch paths that must not grow their own spawn.
const CALLERS: &[(&str, &str)] = &[
    ("mcp.rs (session discovery)", MCP_SRC),
    ("research.rs (designated search)", RESEARCH_SRC),
    ("doctor.rs (connectivity probe)", DOCTOR_SRC),
];

#[test]
fn no_launch_path_spawns_an_mcp_server_itself() {
    for (label, source) in CALLERS {
        assert!(
            !source.contains("StdioTransport::spawn"),
            "{label} spawns an MCP server directly; launch through \
             `mcp_env::spawn_server` so every path resolves the environment the \
             same way"
        );
    }
}

#[test]
fn every_launch_path_uses_the_shared_seam() {
    for (label, source) in CALLERS {
        assert!(
            source.contains("spawn_server("),
            "{label} should launch through `mcp_env::spawn_server`"
        );
    }
}

/// The seam resolves before it spawns, so a server whose credential is missing
/// never becomes a running process. Starting it anyway would turn a
/// configuration error into an obscure runtime failure from a server that came
/// up without the value it was configured to need.
#[test]
fn the_seam_resolves_before_it_spawns() {
    let resolve = SEAM_SRC
        .find("let environment = resolve_environment(server, store)?;")
        .expect("the seam resolves the environment");
    let spawn = SEAM_SRC
        .find("StdioTransport::spawn(&server.command")
        .expect("the seam spawns the server");
    assert!(
        resolve < spawn,
        "resolution must precede the spawn so a missing credential starts no process"
    );
}
