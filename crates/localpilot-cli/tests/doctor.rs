#[allow(dead_code)]
#[path = "../src/doctor.rs"]
mod doctor;

// `doctor` references `crate::output::OutputFormat`; include the same module so the
// standalone test build of `doctor.rs` resolves it (it is otherwise the bin crate's).
#[allow(dead_code)]
#[path = "../src/output.rs"]
mod output;

use doctor::{ConfigPath, DoctorReport, ProviderStatus, ToolStatus, TrustState};
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
        memory_root: Some("<memory-root>".to_string()),
        capabilities: vec![
            "doctor-json".to_string(),
            "models-json".to_string(),
            "learning-workspace-flag".to_string(),
            "print-turn-timeout".to_string(),
        ],
        workspace_trust: TrustState::Unknown,
    }
}
