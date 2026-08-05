#[allow(dead_code)]
#[path = "../src/doctor.rs"]
mod doctor;

// `doctor` references `crate::output::OutputFormat` and `crate::mcp_env`; include
// the same modules so the standalone test build of `doctor.rs` resolves them
// (they are otherwise the bin crate's).
#[allow(dead_code)]
#[path = "../src/mcp_env.rs"]
mod mcp_env;
#[allow(dead_code)]
#[path = "../src/output.rs"]
mod output;
// `doctor` now reads the trust store through `crate::trust`; include it too.
#[allow(dead_code)]
#[path = "../src/trust.rs"]
mod trust;

use doctor::{
    AgentsStatus, ConfigPath, DoctorReport, McpServerState, McpServerStatus, ProviderStatus,
    ToolStatus, TrustState,
};
use localpilot_config::CredentialSource;

#[test]
fn doctor_reports_foundation_status() {
    let report = report();
    let rendered = doctor::render(&report).trim_end_matches('\n').to_string();

    let expected = include_str!("snapshots/doctor.snap").trim_end_matches('\n');
    assert_eq!(rendered, expected);
}

#[test]
fn doctor_does_not_print_secret_values() {
    let mut report = report();
    report.providers = vec![ProviderStatus {
        name: "openai".to_string(),
        kind: "openai".to_string(),
        base_url: None,
        credential_env: "OPENAI_API_KEY".to_string(),
        credential_source: CredentialSource::Env,
        model: None,
        context_window: None,
        supports_vision: None,
    }];

    let rendered = doctor::render(&report);

    assert!(rendered.contains("OPENAI_API_KEY [env]"));
    assert!(!rendered.contains("secret-from-config"));
    assert!(!rendered.contains("secret-from-env"));
}

#[test]
fn doctor_renders_google_adc_source_without_file_contents() {
    let mut report = report();
    report.providers = vec![ProviderStatus {
        name: "gemini".to_string(),
        kind: "google-vertex-openai".to_string(),
        base_url: None,
        credential_env: "GOOGLE_APPLICATION_CREDENTIALS".to_string(),
        credential_source: CredentialSource::GoogleAdcFile,
        model: Some("google/gemini-3.5-flash".to_string()),
        context_window: None,
        supports_vision: None,
    }];

    let rendered = doctor::render(&report);

    assert!(rendered.contains("GOOGLE_APPLICATION_CREDENTIALS [google_adc_file]"));
    assert!(!rendered.contains("application_default_credentials"));
    assert!(!rendered.contains("refresh_token"));
}

#[test]
fn doctor_reports_mcp_servers_without_printing_raw_args() {
    let mut report = report();
    report.mcp_servers = vec![
        McpServerStatus {
            name: "context7".to_string(),
            command: "npx".to_string(),
            arg_count: 2,
            command_available: true,
            connected: true,
            protocol_version: Some("2025-06-18".to_string()),
            tool_count: 2,
            env_names: Vec::new(),
            state: doctor::McpServerState::Connected,
            tools: vec![
                "resolve-library-id".to_string(),
                "get-library-docs".to_string(),
            ],
            error: None,
        },
        McpServerStatus {
            name: "playwright".to_string(),
            command: "npx".to_string(),
            arg_count: 3,
            command_available: true,
            connected: false,
            protocol_version: None,
            tool_count: 0,
            env_names: Vec::new(),
            state: doctor::McpServerState::StartupFailed,
            tools: Vec::new(),
            error: Some("spawn npx: token [REDACTED] failed".to_string()),
        },
    ];

    let rendered = doctor::render(&report);

    assert!(rendered.contains("mcp servers:"));
    assert!(rendered.contains("context7 (npx): connected; protocol 2025-06-18; 2 tool(s): resolve-library-id, get-library-docs"));
    assert!(rendered.contains("failed to start; spawn npx: token [REDACTED] failed"));
    assert!(rendered.contains("args: 2"));
    assert!(!rendered.contains("@upstash/context7-mcp"));
    assert!(!rendered.contains("secret-from-arg"));
}

#[test]
fn doctor_json_includes_mcp_servers() {
    let mut report = report();
    report.mcp_servers = vec![McpServerStatus {
        name: "context7".to_string(),
        command: "npx".to_string(),
        arg_count: 2,
        command_available: true,
        connected: true,
        protocol_version: Some("2025-06-18".to_string()),
        tool_count: 1,
        env_names: Vec::new(),
        state: doctor::McpServerState::Connected,
        tools: vec!["get-library-docs".to_string()],
        error: None,
    }];

    let json: serde_json::Value =
        serde_json::from_str(&doctor::render_json(&report)).expect("doctor JSON parses");

    assert_eq!(json["mcp_servers"][0]["name"], "context7");
    assert_eq!(json["mcp_servers"][0]["tools"][0], "get-library-docs");
    assert_eq!(json["mcp_servers"][0]["arg_count"], 2);
}

fn report() -> DoctorReport {
    DoctorReport {
        version: "<version>".to_string(),
        binary_path: Some("<binary>".to_string()),
        os: "<os>".to_string(),
        arch: "<arch>".to_string(),
        config_paths: vec![
            ConfigPath {
                label: "user".to_string(),
                path: "<config-home>/localpilot/config.toml".to_string(),
                exists: false,
            },
            ConfigPath {
                label: "project".to_string(),
                path: "<workspace>/.localpilot.toml".to_string(),
                exists: true,
            },
        ],
        providers: vec![
            ProviderStatus {
                name: "local".to_string(),
                kind: "local".to_string(),
                base_url: None,
                credential_env: "LOCALPILOT_LOCAL_API_KEY".to_string(),
                credential_source: CredentialSource::None,
                model: None,
                context_window: None,
                supports_vision: None,
            },
            ProviderStatus {
                name: "openai".to_string(),
                kind: "openai".to_string(),
                base_url: None,
                credential_env: "OPENAI_API_KEY".to_string(),
                credential_source: CredentialSource::None,
                model: None,
                context_window: None,
                supports_vision: None,
            },
            ProviderStatus {
                name: "anthropic".to_string(),
                kind: "anthropic".to_string(),
                base_url: None,
                credential_env: "ANTHROPIC_API_KEY".to_string(),
                credential_source: CredentialSource::None,
                model: None,
                context_window: None,
                supports_vision: None,
            },
        ],
        tools: vec![
            ToolStatus {
                name: "git".to_string(),
                command: "git".to_string(),
                available: true,
                optional: false,
            },
            ToolStatus {
                name: "ripgrep".to_string(),
                command: "rg".to_string(),
                available: true,
                optional: true,
            },
        ],
        mcp_servers: Vec::new(),
        memory_root: Some("<memory-root>".to_string()),
        research_docs: None,
        agents: AgentsStatus::default(),
        capabilities: vec![
            "doctor-json".to_string(),
            "models-json".to_string(),
            "learning-workspace-flag".to_string(),
            "print-turn-timeout".to_string(),
        ],
        workspace_trust: TrustState::Unknown,
        workspace_trust_store: Some(
            "/home/user/.config/localpilot/trusted-folders.txt".to_string(),
        ),
        hygiene: None,
    }
}

/// One credential value that must never appear in any rendering, whatever state
/// a server is in.
const NEVER_PRINTED: &str = "super-secret-credential-value";

/// Build one server in each of the four states `doctor` distinguishes, each
/// carrying a configured variable name and — in the failure text — everything a
/// careless implementation might have interpolated.
fn four_states() -> Vec<McpServerStatus> {
    let base = |name: &str, state: McpServerState, error: Option<&str>| McpServerStatus {
        name: name.to_string(),
        command: "npx".to_string(),
        arg_count: 1,
        command_available: state != McpServerState::CommandUnavailable,
        connected: state == McpServerState::Connected,
        protocol_version: (state == McpServerState::Connected).then(|| "2025-06-18".to_string()),
        tool_count: 0,
        tools: Vec::new(),
        env_names: vec!["SERVICE_KEY".to_string()],
        state,
        error: error.map(str::to_string),
    };
    vec![
        base(
            "missing-command",
            McpServerState::CommandUnavailable,
            Some("command not found"),
        ),
        base(
            "missing-credential",
            McpServerState::CredentialMissing,
            Some(
                "environment variable SERVICE_KEY needs the credential \"my-alias\",                  which is not stored",
            ),
        ),
        base(
            "wont-start",
            McpServerState::StartupFailed,
            Some("spawn npx: no such file"),
        ),
        base("healthy", McpServerState::Connected, None),
    ]
}

#[test]
fn doctor_distinguishes_the_four_mcp_server_states() {
    let mut report = report();
    report.mcp_servers = four_states();
    let rendered = doctor::render(&report);

    // Each state reads differently, so a user can tell "you never installed the
    // command" from "you never stored the credential" from "it crashed".
    assert!(rendered.contains("missing-command (npx): command not found"));
    assert!(rendered.contains("missing-credential (npx): credential missing"));
    assert!(rendered.contains("wont-start (npx): failed to start"));
    assert!(rendered.contains("healthy (npx): connected"));

    // The credential-missing line names the variable and the alias, which is the
    // whole point of the diagnostic.
    assert!(rendered.contains("SERVICE_KEY"));
    assert!(rendered.contains("my-alias"));

    let json: serde_json::Value =
        serde_json::from_str(&doctor::render_json(&report)).expect("doctor JSON parses");
    let states: Vec<&str> = json["mcp_servers"]
        .as_array()
        .expect("servers array")
        .iter()
        .map(|server| server["state"].as_str().expect("state token"))
        .collect();
    assert_eq!(
        states,
        vec![
            "command_unavailable",
            "credential_missing",
            "startup_failed",
            "connected"
        ]
    );
}

/// The invariant both renderings must enforce identically: a configured value
/// never appears, in any state. Asserted against *both* so one cannot drift into
/// printing what the other masks.
#[test]
fn no_rendering_prints_a_configured_environment_value() {
    let mut report = report();
    report.mcp_servers = four_states();
    // Simulate the worst case: a server whose failure text somehow carried the
    // value. Both renderings must still be clean of it after redaction upstream,
    // and neither may add it back from `env_names`.
    for server in &mut report.mcp_servers {
        server.env_names.push("ANOTHER_KEY".to_string());
    }

    let human = doctor::render(&report);
    let json = doctor::render_json(&report);

    for rendering in [&human, &json] {
        assert!(
            !rendering.contains(NEVER_PRINTED),
            "a credential value reached a doctor rendering: {rendering}"
        );
    }
    // Names are reported in both, so the diagnostic stays actionable.
    assert!(human.contains("SERVICE_KEY"));
    assert!(json.contains("SERVICE_KEY"));
}
