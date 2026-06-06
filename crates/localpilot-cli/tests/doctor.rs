#[allow(dead_code)]
#[path = "../src/doctor.rs"]
mod doctor;

use doctor::{ConfigPath, DoctorReport, ProviderStatus, ToolStatus, TrustState};

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
        credential_env: "OPENAI_API_KEY".to_string(),
        credential_present: true,
    }];

    let rendered = doctor::render(&report);

    assert!(rendered.contains("OPENAI_API_KEY set"));
    assert!(!rendered.contains("secret-from-config"));
    assert!(!rendered.contains("secret-from-env"));
}

fn report() -> DoctorReport {
    DoctorReport {
        version: "<version>".to_string(),
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
                credential_env: "LOCALPILOT_LOCAL_API_KEY".to_string(),
                credential_present: false,
            },
            ProviderStatus {
                name: "openai".to_string(),
                credential_env: "OPENAI_API_KEY".to_string(),
                credential_present: false,
            },
            ProviderStatus {
                name: "anthropic".to_string(),
                credential_env: "ANTHROPIC_API_KEY".to_string(),
                credential_present: false,
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
        workspace_trust: TrustState::Unknown,
    }
}
