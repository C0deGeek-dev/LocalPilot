//! Configuration precedence, redaction, and diagnostics tests.
//!
//! These cover the required MVP config tests: default loads, project overrides
//! user, env overrides project, CLI overrides env, and secrets stay out of debug
//! output. `figment::Jail` isolates the environment and working directory so the
//! tests never touch a real home or config directory.

use figment::Jail;
use proptest::prelude::*;
use unshackled_config::{load, CliOverrides, ConfigPaths};

fn write(jail: &Jail, name: &str, contents: &str) -> Result<std::path::PathBuf, figment::Error> {
    let path = jail.directory().join(name);
    std::fs::write(&path, contents).map_err(|e| figment::Error::from(e.to_string()))?;
    Ok(path)
}

#[test]
fn default_config_loads() {
    Jail::expect_with(|_jail| {
        let cfg = load(&ConfigPaths::default(), &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.provider.default, "local");
        assert_eq!(cfg.harness.attempts_per_step, 3);
        assert!(cfg.harness.auto_commit);
        assert_eq!(cfg.quota.max_wait_minutes, 360);
        Ok(())
    });
}

#[test]
fn project_overrides_user() {
    Jail::expect_with(|jail| {
        let user = write(jail, "user.toml", "[provider]\ndefault = \"openai\"\n")?;
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        let paths = ConfigPaths {
            user: Some(user),
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.provider.default, "local");
        Ok(())
    });
}

#[test]
fn env_overrides_project() {
    Jail::expect_with(|jail| {
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        jail.set_env("UNSHACKLED_PROVIDER__DEFAULT", "envprovider");
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.provider.default, "envprovider");
        Ok(())
    });
}

#[test]
fn cli_overrides_env() {
    Jail::expect_with(|jail| {
        let project = write(jail, "project.toml", "[provider]\ndefault = \"local\"\n")?;
        jail.set_env("UNSHACKLED_PROVIDER__DEFAULT", "envprovider");
        let cli = CliOverrides {
            provider_default: Some("cliprovider".to_string()),
            ..CliOverrides::default()
        };
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &cli).map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.provider.default, "cliprovider");
        Ok(())
    });
}

#[test]
fn secrets_never_appear_in_debug_output() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.openai]\nkind = \"openai\"\napi_key_env = \"OPENAI_API_KEY\"\n",
        )?;
        jail.set_env("OPENAI_API_KEY", "sk-super-secret-value");
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;

        // The config never holds the credential, only the env-var name.
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("sk-super-secret-value"));
        assert!(debug.contains("OPENAI_API_KEY"));

        // The resolved credential exposes its value only on purpose.
        let secret = cfg
            .resolve_credential("openai")
            .expect("credential present");
        assert_eq!(secret.expose(), "sk-super-secret-value");
        assert!(!format!("{secret:?}").contains("sk-super-secret-value"));
        Ok(())
    });
}

#[test]
fn namespaced_provider_options_are_preserved() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:11434/v1\"\nreasoning_effort = \"high\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
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
    });
}

#[test]
fn resolve_model_uses_the_default_provider_when_unspecified() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[provider]\ndefault = \"local\"\n\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\nmodel = \"local-coder\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.resolve_model(None).as_deref(), Some("local-coder"));
        assert_eq!(
            cfg.resolve_model(Some("local")).as_deref(),
            Some("local-coder")
        );
        assert_eq!(cfg.resolve_model(Some("absent")), None);
        Ok(())
    });
}

#[test]
fn resolve_model_is_none_without_a_configured_model() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.resolve_model(None), None);
        Ok(())
    });
}

#[test]
fn provider_env_fallbacks_resolve_public_env_names() {
    Jail::expect_with(|jail| {
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
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(
            cfg.resolve_credential("anthropic")
                .expect("credential present")
                .expose(),
            "anthropic-secret"
        );
        assert_eq!(cfg.resolve_model(None).as_deref(), Some("claude-local"));
        Ok(())
    });
}

#[test]
fn provider_timeout_and_thinking_config_parse() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"http://localhost:8080/v1\"\nrequest_timeout_secs = 600\nsuppress_thinking = true\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        let local = cfg.providers.get("local").expect("provider present");
        assert_eq!(local.request_timeout_secs, Some(600));
        assert_eq!(local.suppress_thinking, Some(true));
        Ok(())
    });
}

#[test]
fn unknown_keys_are_ignored_for_forward_compatibility() {
    Jail::expect_with(|jail| {
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
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        assert_eq!(cfg.provider.default, "local");
        Ok(())
    });
}

#[test]
fn mcp_servers_parse_with_command_and_args() {
    Jail::expect_with(|jail| {
        let project = write(
            jail,
            "project.toml",
            "[mcp.servers.files]\ncommand = \"my-mcp-server\"\nargs = [\"--root\", \".\"]\n",
        )?;
        let paths = ConfigPaths {
            user: None,
            project: Some(project),
        };
        let cfg = load(&paths, &CliOverrides::default())
            .map_err(|e| figment::Error::from(e.to_string()))?;
        let server = cfg.mcp.servers.get("files").expect("server present");
        assert_eq!(server.command, "my-mcp-server");
        assert_eq!(server.args, vec!["--root".to_string(), ".".to_string()]);
        Ok(())
    });
}

#[test]
fn invalid_config_names_the_offending_key() {
    Jail::expect_with(|jail| {
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
    });
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
            .or_else(|| std::env::var("UNSHACKLED_PROVIDER__DEFAULT").ok())
            .or(project)
            .or(user)
            .unwrap_or_else(|| "local".to_string());
        prop_assert_eq!(cfg.provider.default, expected);
    }
}
