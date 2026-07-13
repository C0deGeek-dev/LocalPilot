//! Configuration precedence, redaction, and diagnostics tests.
//!
//! These cover the required MVP config tests: default loads, project overrides
//! user, env overrides project, CLI overrides env, and secrets stay out of debug
//! output. The local isolation helper keeps environment-variable tests from
//! touching the real process environment outside each test body.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use localpilot_config::{load, AutoFix, Cadence, CliOverrides, ConfigPaths, LookupPolicy};
use proptest::prelude::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const ENV_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_MODEL",
    "OPENAI_API_KEY",
    "OPENAI_BASE_URL",
    "OPENAI_MODEL",
    "LOCALPILOT_PROVIDER__DEFAULT",
];

struct TestEnv {
    _guard: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    original_cwd: PathBuf,
    saved_env: Vec<(&'static str, Option<OsString>)>,
}

impl TestEnv {
    fn new() -> TestResult<Self> {
        // Recover from a poisoned lock: the guard protects only `()`, so a prior
        // test panicking while holding it must not cascade-fail unrelated tests.
        let guard = env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir()?;
        let original_cwd = std::env::current_dir()?;
        let saved_env = ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        std::env::set_current_dir(dir.path())?;
        Ok(Self {
            _guard: guard,
            dir,
            original_cwd,
            saved_env,
        })
    }

    fn directory(&self) -> &Path {
        self.dir.path()
    }

    fn set_env(&self, key: &str, value: &str) {
        std::env::set_var(key, value);
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original_cwd);
        for (key, value) in &self.saved_env {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn isolated(test: impl FnOnce(&TestEnv) -> TestResult) -> TestResult {
    let env = TestEnv::new()?;
    test(&env)
}

fn write(env: &TestEnv, name: &str, contents: &str) -> TestResult<PathBuf> {
    let path = env.directory().join(name);
    std::fs::write(&path, contents)?;
    Ok(path)
}

#[test]
fn synthesizes_an_anthropic_provider_from_env_when_unconfigured() -> TestResult {
    // A launcher that exports the documented Anthropic env vars (and no config
    // file) should produce a usable default provider — the drop-in path.
    isolated(|jail| {
        jail.set_env("ANTHROPIC_BASE_URL", "http://127.0.0.1:11435");
        jail.set_env("ANTHROPIC_MODEL", "local-model");
        // Gateway auth via AUTH_TOKEN; API_KEY is empty.
        jail.set_env("ANTHROPIC_API_KEY", "");
        jail.set_env("ANTHROPIC_AUTH_TOKEN", "secret");

        let cfg = load(&ConfigPaths::default(), &CliOverrides::default())?;

        // The env provider is registered under the existing default id without
        // changing `provider.default`.
        let id = cfg.provider.default.clone();
        let provider = cfg.providers.get(&id).expect("synthesized provider");
        assert_eq!(provider.kind, "anthropic");
        assert_eq!(provider.base_url.as_deref(), Some("http://127.0.0.1:11435"));
        assert_eq!(cfg.resolve_model(None).as_deref(), Some("local-model"));
        // Credential falls back to AUTH_TOKEN when API_KEY is empty.
        assert_eq!(
            cfg.resolve_credential(&id).map(|s| s.expose().to_string()),
            Some("secret".to_string())
        );
        Ok(())
    })
}

#[test]
fn default_config_loads() -> TestResult {
    isolated(|_jail| {
        let cfg = load(&ConfigPaths::default(), &CliOverrides::default())?;
        assert_eq!(cfg.provider.default, "local");
        assert_eq!(cfg.harness.attempts_per_step, 3);
        assert!(cfg.harness.auto_commit);
        assert!(cfg.context.project_analysis);
        assert_eq!(cfg.docs.lookup_policy, LookupPolicy::Evidence);
        assert_eq!(cfg.quota.max_wait_minutes, 360);
        Ok(())
    })
}

#[test]
fn project_analysis_and_lookup_policy_parse() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[context]\nproject_analysis = false\n\n[docs]\nlookup_policy = \"proactive\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert!(!cfg.context.project_analysis);
        assert_eq!(cfg.docs.lookup_policy, LookupPolicy::Proactive);
        Ok(())
    })
}

#[test]
fn project_overrides_user() -> TestResult {
    isolated(|jail| {
        let user = write(jail, "user.toml", "[provider]\ndefault = \"openai\"\n")?;
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        let paths = ConfigPaths {
            user: Some(user),
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.provider.default, "local");
        Ok(())
    })
}

#[test]
fn env_overrides_project() -> TestResult {
    isolated(|jail| {
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        jail.set_env("LOCALPILOT_PROVIDER__DEFAULT", "envprovider");
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.provider.default, "envprovider");
        Ok(())
    })
}

#[test]
fn cli_overrides_env() -> TestResult {
    isolated(|jail| {
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        jail.set_env("LOCALPILOT_PROVIDER__DEFAULT", "envprovider");
        let cli = CliOverrides {
            provider_default: Some("cliprovider".to_string()),
            ..CliOverrides::default()
        };
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &cli)?;
        assert_eq!(cfg.provider.default, "cliprovider");
        Ok(())
    })
}

#[test]
fn secrets_never_appear_in_debug_output() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.secret_test]\nkind = \"openai\"\napi_key_env = \"LOCALPILOT_TEST_SECRET_KEY\"\n",
        )?;
        jail.set_env("LOCALPILOT_TEST_SECRET_KEY", "sk-super-secret-value");
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;

        // The config never holds the credential, only the env-var name.
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("sk-super-secret-value"));
        assert!(debug.contains("LOCALPILOT_TEST_SECRET_KEY"));

        // The resolved credential exposes its value only on purpose.
        let secret = cfg
            .resolve_credential("secret_test")
            .expect("credential present");
        assert_eq!(secret.expose(), "sk-super-secret-value");
        assert!(!format!("{secret:?}").contains("sk-super-secret-value"));
        Ok(())
    })
}

#[test]
fn namespaced_provider_options_are_preserved() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:11434/v1\"\nreasoning_effort = \"high\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        let local = cfg.providers.get("local").expect("provider present");
        assert_eq!(local.kind, "openai-compatible");
        assert_eq!(
            local
                .options
                .get("reasoning_effort")
                .and_then(|v| v.as_str()),
            Some("high")
        );
        Ok(())
    })
}

#[test]
fn resolve_model_uses_the_default_provider_when_unspecified() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\nmodel = \"local-coder\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.resolve_model(None).as_deref(), Some("local-coder"));
        assert_eq!(
            cfg.resolve_model(Some("local")).as_deref(),
            Some("local-coder")
        );
        assert_eq!(cfg.resolve_model(Some("absent")), None);
        Ok(())
    })
}

#[test]
fn resolve_model_is_none_without_a_configured_model() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.resolve_model(None), None);
        Ok(())
    })
}

#[test]
fn provider_env_fallbacks_resolve_public_env_names() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[provider]\ndefault = \"anthropic\"\n\n[providers.anthropic]\nkind = \"anthropic\"\n",
        )?;
        jail.set_env("ANTHROPIC_API_KEY", "anthropic-secret");
        jail.set_env("ANTHROPIC_MODEL", "claude-local");
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(
            cfg.resolve_credential("anthropic")
                .expect("credential present")
                .expose(),
            "anthropic-secret"
        );
        assert_eq!(cfg.resolve_model(None).as_deref(), Some("claude-local"));
        Ok(())
    })
}

#[test]
fn provider_timeout_and_thinking_config_parse() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\nrequest_timeout_secs = 600\nsuppress_thinking = true\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        let local = cfg.providers.get("local").expect("provider present");
        assert_eq!(local.request_timeout_secs, Some(600));
        assert_eq!(local.suppress_thinking, Some(true));
        // Vision is unset by default: an undeclared local provider keeps today's
        // text-only behaviour.
        assert_eq!(local.supports_vision, None);
        Ok(())
    })
}

#[test]
fn provider_supports_vision_parses() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\nsupports_vision = true\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        let local = cfg.providers.get("local").expect("provider present");
        assert_eq!(local.supports_vision, Some(true));
        Ok(())
    })
}

#[test]
fn vision_probe_defaults_on_and_is_toggleable() -> TestResult {
    isolated(|jail| {
        // Absent `[discovery]` keeps the probe on (best-effort, read-only).
        let default_cfg = load(
            &ConfigPaths {
                user: None,
                project: None,
            },
            &CliOverrides::default(),
        )?;
        assert!(default_cfg.discovery.vision_probe);

        let project = write(jail, "project.toml", "[discovery]\nvision_probe = false\n")?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert!(!cfg.discovery.vision_probe);
        Ok(())
    })
}

#[test]
fn google_adc_provider_config_parses_without_static_secret() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[provider]\ndefault = \"gemini\"\n\n\
             [providers.gemini]\n\
             kind = \"google-vertex-openai\"\n\
             auth = \"google_adc\"\n\
             google_project = \"my-project\"\n\
             google_location = \"global\"\n\
             google_adc_path = \"/Users/bramhammer/.config/gcloud/application_default_credentials.json\"\n\
             model = \"google/gemini-3.5-flash\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        let provider = cfg.providers.get("gemini").expect("provider present");

        assert_eq!(provider.kind, "google-vertex-openai");
        assert_eq!(provider.auth, localpilot_config::ProviderAuth::GoogleAdc);
        assert_eq!(provider.google_project.as_deref(), Some("my-project"));
        assert_eq!(provider.google_location.as_deref(), Some("global"));
        assert_eq!(
            provider.google_adc_path.as_deref(),
            Some("/Users/bramhammer/.config/gcloud/application_default_credentials.json")
        );
        assert!(cfg.resolve_credential("gemini").is_none());
        assert_eq!(
            cfg.credential_source("gemini"),
            localpilot_config::CredentialSource::GoogleAdcFile
        );
        Ok(())
    })
}

#[test]
fn unknown_keys_are_ignored_for_forward_compatibility() -> TestResult {
    isolated(|jail| {
        // A config written for a newer version (unknown top-level table and an
        // unknown key in a known table) must still load on this binary.
        let project = write(
            jail,
            "project.toml",
            "[provider]\ndefault = \"local\"\nfuture_field = true\n\n[future_section]\nanything = 1\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.provider.default, "local");
        Ok(())
    })
}

#[test]
fn mcp_servers_parse_with_command_and_args() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[mcp.servers.files]\ncommand = \"my-mcp-server\"\nargs = [\"--root\", \".\"]\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        let server = cfg.mcp.servers.get("files").expect("server present");
        assert_eq!(server.command, "my-mcp-server");
        assert_eq!(server.args, vec!["--root".to_string(), ".".to_string()]);
        Ok(())
    })
}

#[test]
fn ingest_config_defaults_are_conservative() -> TestResult {
    isolated(|_jail| {
        let cfg = load(&ConfigPaths::default(), &CliOverrides::default())?;

        assert!(cfg.ingest.enabled);
        assert!(cfg.ingest.default_skip_dirs.contains(&"target".to_string()));
        assert!(cfg
            .ingest
            .default_skip_dirs
            .contains(&".localmind".to_string()));
        assert_eq!(cfg.ingest.max_model_calls, 0);
        Ok(())
    })
}

#[test]
fn project_ingest_config_overrides_defaults() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            r#"
[ingest]
enabled = true
include = ["docs"]
exclude = ["secrets"]
default_skip_dirs = ["target", "node_modules", ".cache"]
max_file_bytes = 4096
max_run_bytes = 8192
max_files = 7
max_tokens = 1234
max_elapsed_secs = 15
max_model_calls = 2
"#,
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;

        assert!(cfg.ingest.enabled);
        assert_eq!(cfg.ingest.include, vec!["docs"]);
        assert_eq!(cfg.ingest.exclude, vec!["secrets"]);
        assert_eq!(
            cfg.ingest.default_skip_dirs,
            vec!["target", "node_modules", ".cache"]
        );
        assert_eq!(cfg.ingest.max_file_bytes, 4096);
        assert_eq!(cfg.ingest.max_run_bytes, 8192);
        assert_eq!(cfg.ingest.max_files, 7);
        assert_eq!(cfg.ingest.max_tokens, 1234);
        assert_eq!(cfg.ingest.max_elapsed_secs, 15);
        assert_eq!(cfg.ingest.max_model_calls, 2);
        Ok(())
    })
}

#[test]
fn invalid_config_names_the_offending_key() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[harness]\nattempts_per_step = \"three\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let err = load(&paths, &CliOverrides::default()).expect_err("invalid config should fail");
        let message = err.to_string();
        assert!(
            message.contains("attempts_per_step"),
            "diagnostic did not name the key: {message}"
        );
        Ok(())
    })
}

#[test]
fn harness_checks_parse_from_toml() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[[harness.checks]]\nname = \"fmt\"\nprogram = \"cargo\"\nargs = [\"fmt\", \"--check\"]\nauto_fix = true\n\n[[harness.checks]]\nname = \"test\"\nprogram = \"cargo\"\nargs = [\"test\"]\ncadence = \"phase\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert_eq!(cfg.harness.checks.len(), 2);

        let fmt = &cfg.harness.checks[0];
        assert_eq!(fmt.name, "fmt");
        assert_eq!(fmt.program, "cargo");
        assert_eq!(fmt.args, vec!["fmt".to_string(), "--check".to_string()]);
        assert_eq!(fmt.auto_fix, AutoFix::Full);
        assert_eq!(fmt.cadence, Cadence::Step);

        let test = &cfg.harness.checks[1];
        assert_eq!(test.cadence, Cadence::Phase);
        assert_eq!(test.auto_fix, AutoFix::No);
        Ok(())
    })
}

#[test]
fn duplicate_check_names_are_rejected() -> TestResult {
    isolated(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[[harness.checks]]\nname = \"fmt\"\nprogram = \"cargo\"\n\n[[harness.checks]]\nname = \"fmt\"\nprogram = \"cargo\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let err =
            load(&paths, &CliOverrides::default()).expect_err("duplicate check name should fail");
        assert!(
            err.to_string().contains("duplicate check name"),
            "unexpected error: {err}"
        );
        Ok(())
    })
}

proptest! {
    /// Precedence invariant: the highest layer that sets `provider.default` wins,
    /// falling back to the built-in default. Env is exercised separately to keep
    /// this property free of process-global state.
    #[test]
    fn precedence_picks_the_highest_set_layer(
        user in proptest::option::of("[a-z]{1,8}"),
        project in proptest::option::of("[a-z]{1,8}"),
        cli in proptest::option::of("[a-z]{1,8}"),
    ) {
        let _env = TestEnv::new().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut paths = ConfigPaths::default();
        if let Some(u) = &user {
            let p = dir.path().join("user.toml");
            std::fs::write(&p, format!("[provider]\ndefault = \"{u}\"\n")).unwrap();
            paths.user = Some(p);
        }
        if let Some(pr) = &project {
            let p = dir.path().join("project.toml");
            std::fs::write(&p, format!("[provider]\ndefault = \"{pr}\"\n")).unwrap();
            paths.project = Some(p);
        }
        let cli_overrides = CliOverrides {
            provider_default: cli.clone(),
            ..CliOverrides::default()
        };
        let cfg = load(&paths, &cli_overrides).unwrap();
        let expected = cli
            .or(project)
            .or(user)
            .unwrap_or_else(|| "local".to_string());
        prop_assert_eq!(cfg.provider.default, expected);
    }
}

#[test]
fn guidance_gate_defaults_off_and_parses_from_toml() -> TestResult {
    isolated(|jail| {
        // Defaults: the gate ships off with a sane threshold and question cap.
        let empty = write(jail, "empty.toml", "")?;
        let paths = ConfigPaths {
            user: None,
            project: Some(empty),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert!(!cfg.harness.guidance.enabled);
        assert!((cfg.harness.guidance.threshold - 0.7).abs() < f32::EPSILON);
        assert_eq!(cfg.harness.guidance.max_questions, 5);

        // A project config overrides every field.
        let project = write(
            jail,
            "project.toml",
            "[harness.guidance]\nenabled = true\nthreshold = 0.5\nmax_questions = 3\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())?;
        assert!(cfg.harness.guidance.enabled);
        assert!((cfg.harness.guidance.threshold - 0.5).abs() < f32::EPSILON);
        assert_eq!(cfg.harness.guidance.max_questions, 3);
        Ok(())
    })
}
