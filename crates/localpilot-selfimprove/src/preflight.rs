//! Static proposal review. Only committed base configuration can select checks;
//! toolchain discovery reads installed files, never probes a proposed executable.

use localpilot_config::{CheckConfig, Config};
use localpilot_patchgen::validation::{
    ContentDigest, HostToolchainIdentity, PlannedCheck, ProposalInspection, RepositoryAuthority,
    ValidationError, ValidationLimits, ValidationPlanIdentity,
};
use localpilot_patchgen::ProposedPatch;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Conservative source-risk categories, separate from the sandbox's existing
/// command classification. These change warnings, never execution authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightRisk {
    /// Manifests, dependency locks and toolchain selection.
    Dependencies,
    /// Build scripts or proc-macro declarations.
    BuildCode,
    /// Tests, fixtures and support executables.
    TestCode,
    /// Shell, PowerShell, batch and other project scripts.
    Scripts,
    /// CI and trusted validation/tool configuration.
    Configuration,
    /// Native process, network or filesystem access in changed source.
    HostEffects,
    /// Binary or likely generated source.
    BinaryOrGenerated,
}

/// Fully resolved static review. It is data, not execution or merge consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightReport {
    /// Current source and bounded diff, obtained through patchgen inspection.
    pub proposal: ProposalInspection,
    /// Exact commands and limits that a future execution grant will bind.
    pub plan: ValidationPlanIdentity,
    /// Frozen host binaries/selector; no raw private configuration is stored.
    pub toolchain: HostToolchainIdentity,
    /// Conservative risks and their exact changed paths.
    pub risks: BTreeMap<PreflightRisk, Vec<String>>,
    /// Existing sandbox command classes, displayed without inventing a new policy.
    pub command_classes: Vec<String>,
}

/// Resolve a read-only review of a persisted proposal. No token is issued, no
/// proposed code is run, and failed inspection leaves the proposal on disk.
///
/// # Errors
/// Missing committed checks/toolchain, unsupported registered commands, missing
/// installed tools, dirty/stale source, authority contention or invalid metadata.
pub fn proposal_preflight(root: &Path, id: &str) -> Result<PreflightReport, ValidationError> {
    let mut authority = RepositoryAuthority::open(root)?;
    let transaction = authority.try_transaction()?;
    let patch = ProposedPatch::reopen(root, id)?;
    preflight_in_transaction(root, &patch, &transaction)
}

pub(crate) fn preflight_in_transaction(
    root: &Path,
    patch: &ProposedPatch,
    transaction: &localpilot_patchgen::validation::RepositoryTransaction<'_>,
) -> Result<PreflightReport, ValidationError> {
    let checks = base_checks(patch)?;
    let selector = toolchain_selector(patch)?;
    let root = fs::canonicalize(root).map_err(|e| io(root, e))?;
    let worktree =
        fs::canonicalize(patch.worktree_path()).map_err(|e| io(patch.worktree_path(), e))?;
    let host = resolve_host(&selector, &checks, &[&root, &worktree])?;
    build_report(patch, transaction, &checks, host)
}

fn base_checks(patch: &ProposedPatch) -> Result<Vec<CheckConfig>, ValidationError> {
    let config = patch.validation_base_text(".localpilot.toml")?
        .ok_or_else(|| invalid("no committed validation checks in the proposal base; review a command-only project configuration before creating a new proposal (keep credentials in user/environment configuration)"))?;
    // Do not expose a TOML error's raw source excerpt: provider settings may
    // contain secrets even though only harness checks are used here.
    let config: Config = toml::from_str(&config).map_err(|_| {
        invalid("committed validation configuration is not valid TOML/configuration")
    })?;
    if config.harness.checks.is_empty() || config.harness.checks.len() > 16 {
        return Err(invalid(
            "validation requires1–16 explicitly ratified base checks",
        ));
    }
    Ok(config.harness.checks)
}

fn toolchain_selector(patch: &ProposedPatch) -> Result<String, ValidationError> {
    let text = patch
        .validation_base_text("rust-toolchain.toml")?
        .ok_or_else(|| {
            invalid("registered Rust validation requires rust-toolchain.toml in the proposal base")
        })?;
    #[derive(Deserialize)]
    struct File {
        toolchain: Toolchain,
    }
    #[derive(Deserialize)]
    struct Toolchain {
        channel: String,
    }
    let parsed: File =
        toml::from_str(&text).map_err(|_| invalid("invalid trusted Rust toolchain selector"))?;
    let channel = parsed.toolchain.channel;
    if channel.is_empty()
        || channel.len() > 128
        || channel.starts_with('-')
        || !channel
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_'))
    {
        return Err(invalid("unsupported trusted Rust toolchain selector"));
    }
    Ok(channel)
}

struct HostRust {
    rustup: PathBuf,
    identity: HostToolchainIdentity,
    private_config_paths: Vec<String>,
}

fn resolve_host(
    selector: &str,
    checks: &[CheckConfig],
    roots: &[&Path],
) -> Result<HostRust, ValidationError> {
    let rustup = resolve_program("rustup", roots)?;
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE")
    } else {
        std::env::var_os("HOME")
    }
    .ok_or_else(|| invalid("host home directory unavailable for installed toolchain discovery"))?;
    let rustup_home = std::env::var_os("RUSTUP_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".rustup"));
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".cargo"));
    if !rustup_home.is_absolute() || !cargo_home.is_absolute() {
        return Err(invalid("toolchain/cache roots must be absolute"));
    }
    let target = host_target()?;
    // Do not guess another installed tune/toolchain or install a missing one.
    let directory = if selector.ends_with(&target) {
        selector.to_owned()
    } else {
        format!("{selector}-{target}")
    };
    let installed = fs::canonicalize(rustup_home.join("toolchains").join(directory))
        .map_err(|_| invalid("the exact base Rust toolchain is not installed; no automatic installation or fallback"))?;
    if roots.iter().any(|root| installed.starts_with(root)) {
        return Err(invalid("toolchain is controlled by the proposal/project"));
    }
    let mut components = BTreeMap::new();
    components.insert("rustup".into(), host_file_digest(&rustup, roots)?);
    let mut names = BTreeSet::from(["cargo", "rustc", "rustdoc"]);
    for check in checks {
        validate_registered_check(check)?;
        match check.args[0].as_str() {
            "fmt" => {
                names.insert("cargo-fmt");
                names.insert("rustfmt");
            }
            "clippy" => {
                names.insert("cargo-clippy");
                names.insert("clippy-driver");
            }
            _ => {}
        }
    }
    for name in names {
        let path = installed.join("bin").join(executable(name));
        components.insert(format!("bin/{name}"), host_file_digest(&path, roots)?);
    }
    // Rustc is a small loader on several platforms; bind its real driver too.
    for directory in [installed.join("bin"), installed.join("lib")] {
        if !directory.is_dir() {
            continue;
        }
        for entry in fs::read_dir(&directory).map_err(|e| io(&directory, e))? {
            let entry = entry.map_err(|e| io(&directory, e))?;
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 toolchain component"))?;
            if name.contains("rustc_driver") && entry.path().is_file() {
                let relative = entry
                    .path()
                    .strip_prefix(&installed)
                    .map_err(|_| invalid("invalid toolchain path"))?
                    .to_string_lossy()
                    .replace('\\', "/");
                components.insert(relative, host_file_digest(&entry.path(), roots)?);
            }
        }
    }
    let private_config_paths = [cargo_home.join("config"), cargo_home.join("config.toml")]
        .iter()
        .map(|p| {
            p.to_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("non-UTF8 host configuration path"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(HostRust {
        rustup,
        identity: HostToolchainIdentity {
            selector: selector.into(),
            root: installed
                .to_str()
                .ok_or_else(|| invalid("non-UTF8 toolchain root"))?
                .into(),
            components,
        },
        private_config_paths,
    })
}

fn validate_registered_check(check: &CheckConfig) -> Result<(), ValidationError> {
    if !matches!(check.program.as_str(), "cargo" | "cargo.exe")
        || !matches!(
            check.args.first().map(String::as_str),
            Some("check" | "build" | "test" | "clippy" | "fmt")
        )
        || check.args.iter().any(|arg| arg == "--fix")
        || check
            .args
            .iter()
            .any(|arg| localpilot_config::redact::redact(arg) != *arg)
        || (check.args.first().map(String::as_str) == Some("fmt")
            && !check.args.iter().any(|arg| arg == "--check"))
    {
        return Err(invalid("profile supports ratified cargo check/build/test/clippy and fmt --check only; no aliases, fixers or arbitrary executables"));
    }
    Ok(())
}

fn build_report(
    patch: &ProposedPatch,
    transaction: &localpilot_patchgen::validation::RepositoryTransaction<'_>,
    checks: &[CheckConfig],
    host: HostRust,
) -> Result<PreflightReport, ValidationError> {
    let workspace = localpilot_sandbox::Workspace::new(patch.worktree_path())
        .map_err(|_| invalid("proposal workspace cannot be resolved"))?;
    let target = workspace
        .resolve(Path::new("target"))
        .map_err(|_| invalid("declared target cache escapes the proposal workspace"))?;
    if target != workspace.root().join("target") {
        return Err(invalid("declared target cache is aliased"));
    }
    let program = host
        .rustup
        .to_str()
        .ok_or_else(|| invalid("non-UTF8 host program"))?
        .to_owned();
    let executable = host
        .identity
        .components
        .get("rustup")
        .ok_or_else(|| invalid("missing host executable identity"))?;
    let mut planned = Vec::new();
    for check in checks {
        validate_registered_check(check)?;
        let mut args = vec!["run".into(), host.identity.selector.clone(), "cargo".into()];
        args.extend(check.args.clone());
        if check.args[0] != "fmt" && !check.args.iter().any(|arg| arg == "--frozen") {
            // Cargo's explicit offline+locked contract; never host containment.
            args.insert(4, "--frozen".into());
        }
        planned.push(PlannedCheck {
            name: check.name.clone(),
            program: program.clone(),
            executable: executable.clone(),
            args,
        });
    }
    let mut environment_names: Vec<String> = [
        "CARGO_HOME",
        "HOME",
        "PATH",
        "RUSTUP_HOME",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "WINDIR",
    ]
    .iter()
    .map(|s| (*s).into())
    .collect();
    if cfg!(windows) {
        // MSVC discovery needs Windows installation roots outside a developer
        // prompt; a developer prompt additionally supplies include/library roots.
        // Preserve only named toolchain inputs, never the whole host environment.
        environment_names.extend(
            [
                "ProgramFiles",
                "ProgramFiles(x86)",
                "ProgramW6432",
                "SystemDrive",
                "INCLUDE",
                "LIB",
                "LIBPATH",
                "VCINSTALLDIR",
                "VCToolsInstallDir",
                "VCToolsVersion",
                "VSINSTALLDIR",
                "WindowsSdkDir",
                "WindowsSDKVersion",
                "UniversalCRTSdkDir",
                "UCRTVersion",
            ]
            .iter()
            .map(|name| (*name).into()),
        );
    }
    environment_names.sort();
    let plan = ValidationPlanIdentity {
        profile: "ratified-rust-v1".into(),
        policy_version: "proposal-validation-v1".into(),
        platform: std::env::consts::OS.into(),
        worktree: fs::canonicalize(patch.worktree_path())
            .map_err(|e| io(patch.worktree_path(), e))?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 worktree root"))?
            .into(),
        toolchain: host.identity.digest(),
        checks: planned,
        environment_names,
        environment_set: BTreeMap::from([
            ("RUSTUP_AUTO_INSTALL".into(), "0".into()),
            (
                "CARGO_TARGET_DIR".into(),
                target
                    .to_str()
                    .ok_or_else(|| invalid("non-UTF8 target cache"))?
                    .into(),
            ),
        ]),
        private_config_paths: host.private_config_paths,
        artifact_paths: Vec::new(),
        cache_paths: vec!["target".into()],
        limits: ValidationLimits::default(),
    };
    let inspection = patch.inspect_validation(transaction, &plan)?;
    let mut risks: BTreeMap<PreflightRisk, Vec<String>> = BTreeMap::new();
    for path in &inspection.diff.files {
        for risk in file_risks(path) {
            risks.entry(risk).or_default().push(path.clone());
        }
        let text = fs::read_to_string(patch.worktree_path().join(path)).unwrap_or_default();
        for (risk, words) in [
            (
                PreflightRisk::BuildCode,
                &["proc-macro", "proc_macro", "build ="][..],
            ),
            (
                PreflightRisk::HostEffects,
                &[
                    "std::process",
                    "std::net",
                    "std::fs",
                    "subprocess",
                    "socket",
                    "Command::",
                    "requests.",
                ][..],
            ),
        ] {
            if words.iter().any(|word| text.contains(word)) {
                risks.entry(risk).or_default().push(path.clone());
            }
        }
    }
    for path in &inspection.binary_paths {
        risks
            .entry(PreflightRisk::BinaryOrGenerated)
            .or_default()
            .push(path.clone());
    }
    for paths in risks.values_mut() {
        paths.sort();
        paths.dedup();
    }
    let command_classes = plan
        .checks
        .iter()
        .map(|check| {
            format!(
                "{:?}",
                localpilot_sandbox::classify(&check.program, &check.args)
            )
        })
        .collect();
    Ok(PreflightReport {
        proposal: inspection,
        plan,
        toolchain: host.identity,
        risks,
        command_classes,
    })
}

fn file_risks(path: &str) -> Vec<PreflightRisk> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or("");
    let mut risks = Vec::new();
    if name.ends_with(".lock")
        || matches!(
            name,
            "cargo.toml"
                | "package.json"
                | "pyproject.toml"
                | "requirements.txt"
                | "rust-toolchain.toml"
        )
    {
        risks.push(PreflightRisk::Dependencies);
    }
    if name == "build.rs" {
        risks.push(PreflightRisk::BuildCode);
    }
    if lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.contains("fixture")
        || lower.contains("test_")
    {
        risks.push(PreflightRisk::TestCode);
    }
    if [".sh", ".ps1", ".bat", ".cmd", ".py"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        risks.push(PreflightRisk::Scripts);
    }
    if lower.starts_with(".github/")
        || lower.starts_with(".cargo/")
        || matches!(
            name,
            ".localpilot.toml" | ".gitattributes" | "rustfmt.toml" | ".rustfmt.toml"
        )
    {
        risks.push(PreflightRisk::Configuration);
    }
    if lower.contains("generated") || lower.contains("/gen/") {
        risks.push(PreflightRisk::BinaryOrGenerated);
    }
    risks
}

fn resolve_program(name: &str, roots: &[&Path]) -> Result<PathBuf, ValidationError> {
    let path = std::env::var_os("PATH").ok_or_else(|| invalid("host PATH is unavailable"))?;
    for directory in std::env::split_paths(&path).filter(|p| p.is_absolute()) {
        let candidate = directory.join(executable(name));
        let Ok(candidate) = fs::canonicalize(candidate) else {
            continue;
        };
        if candidate.is_file() && !roots.iter().any(|root| candidate.starts_with(root)) {
            return Ok(candidate);
        }
    }
    Err(invalid(
        "registered host rustup executable is unavailable outside the project",
    ))
}

fn host_file_digest(path: &Path, roots: &[&Path]) -> Result<ContentDigest, ValidationError> {
    let canonical = fs::canonicalize(path).map_err(|e| io(path, e))?;
    if roots.iter().any(|root| canonical.starts_with(root)) || !canonical.is_file() {
        return Err(invalid(
            "proposed/project executable cannot be used as a trusted toolchain probe",
        ));
    }
    let mut bytes = Vec::new();
    fs::File::open(&canonical)
        .map_err(|e| io(&canonical, e))?
        .take(256 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io(&canonical, e))?;
    if bytes.len() > 256 * 1024 * 1024 {
        return Err(invalid("toolchain component exceeds256MiB"));
    }
    Ok(ContentDigest::for_bytes(&bytes))
}

fn host_target() -> Result<String, ValidationError> {
    let arch = std::env::consts::ARCH;
    let suffix = if cfg!(all(windows, target_env = "msvc")) {
        "pc-windows-msvc"
    } else if cfg!(all(windows, target_env = "gnu")) {
        "pc-windows-gnu"
    } else if cfg!(target_os = "macos") {
        "apple-darwin"
    } else if cfg!(all(target_os = "linux", target_env = "gnu")) {
        "unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_env = "musl")) {
        "unknown-linux-musl"
    } else {
        return Err(invalid("unsupported installed-toolchain host layout"));
    };
    Ok(format!("{arch}-{suffix}"))
}

fn executable(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.into()
    }
}
fn invalid(message: &str) -> ValidationError {
    ValidationError::Invalid(message.into())
}
fn io(path: &Path, source: std::io::Error) -> ValidationError {
    ValidationError::Io {
        path: path.to_owned(),
        source,
    }
}

impl PreflightReport {
    /// Exact public preview identity reused by execution consent. Receipts retain
    /// the same proposal/plan pair separately for inspection and rechecks.
    ///
    /// # Errors
    /// Unsupported deserialized plans cannot become approvable by display alone.
    pub fn digest(&self) -> Result<ContentDigest, ValidationError> {
        self.proposal.identity.execution_identity(&self.plan)
    }

    /// Canonical CLI content reused by other hosts without hiding command/risk
    /// details. This preview issues no authority and starts no model/GPU process.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "Proposal {}\nBase: {}\nProposed: {}\nIdentity: {}\nWorktree: {}\n\n{}\n",
            self.proposal.identity.proposal_id,
            self.proposal.identity.base_commit,
            self.proposal.identity.proposed_commit,
            self.proposal.identity.digest().as_str(),
            self.plan.worktree,
            self.proposal.diff.patch
        );
        match self.digest() {
            Ok(identity) => out.push_str(&format!("Preview identity: {}\n", identity.as_str())),
            Err(_) => out.push_str("Preview identity: INVALID — execution cannot be authorized\n"),
        }
        out.push_str("Validation runs proposed code with LocalPilot's host access. Worktree separation protects ordinary edit placement; it does not contain filesystem, credential, network or process effects. LocalPilot itself does not promote or write the main checkout before separate promotion approval. Approved code can reach other host paths; failed runs cannot undo those effects.\n\n");
        out.push_str("Ordered execute-only plan (no fixers or automatic retries):\n");
        for (check, class) in self.plan.checks.iter().zip(&self.command_classes) {
            out.push_str(&format!(
                "  {}: program={:?} argv={:?} class={}\n",
                check.name, check.program, check.args, class
            ));
        }
        out.push_str(&format!("Fixed nonsecret environment: {:?}\nNetwork: Cargo uses --frozen for dependency resolution; proposed code retains host network access.\n", self.plan.environment_set));
        out.push_str(&format!("Environment names: {:?}\nPrivate host config paths (contents not displayed): {:?}\nLimits: {}s/check, {}s total, {} bytes/output stream; artifact count/size/total: {}/{}/{} bytes\nDeclared artifacts: {:?}; caches: {:?}\nCold/warm cost: not measured for this plan. Deadlines are execution limits, not a prediction of runtime.\n", self.plan.environment_names, self.plan.private_config_paths, self.plan.limits.per_check_secs, self.plan.limits.total_secs, self.plan.limits.stream_bytes, self.plan.limits.artifact_count, self.plan.limits.artifact_bytes, self.plan.limits.artifact_total_bytes, self.plan.artifact_paths, self.plan.cache_paths));
        for (risk, paths) in &self.risks {
            out.push_str(&format!("Risk {risk:?}: {paths:?}\n"));
        }
        out.push_str("Risk hints are conservative, not proof that unflagged code is safe. Cleanup must be acknowledged; uncertain cleanup invalidates evidence. No evaluation token issued, checks run, model/GPU launch or promotion performed by this preview.\n");
        localpilot_config::redact::redact(&out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use localpilot_patchgen::{ChangeProvenance, PatchProposal, ProposedEdit};

    fn check(args: &[&str]) -> CheckConfig {
        let mut config = Config::default();
        config.harness.checks.push(CheckConfig {
            name: "test".into(),
            program: "cargo".into(),
            args: args.iter().map(|s| (*s).into()).collect(),
            fix_program: None,
            fix_args: Vec::new(),
            cadence: Default::default(),
            auto_fix: Default::default(),
            severity: None,
        });
        config.harness.checks.remove(0)
    }

    fn fixture() -> (tempfile::TempDir, ProposedPatch) {
        let repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(repo.path())
                .args([
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "commit.gpgSign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "fixture"]);
        git(&["config", "user.email", "fixture@localhost"]);
        git(&["config", "core.autocrlf", "false"]);
        fs::write(repo.path().join(".gitignore"), ".localpilot/\ntarget/\n").unwrap();
        fs::write(
            repo.path().join("Cargo.toml"),
            "[package]\nname='fixture'\nversion='0.0.0'\n",
        )
        .unwrap();
        fs::write(
            repo.path().join("rust-toolchain.toml"),
            "[toolchain]\nchannel='1.82'\n",
        )
        .unwrap();
        fs::write(
            repo.path().join(".localpilot.toml"),
            "[[harness.checks]]\nname='check'\nprogram='cargo'\nargs=['check','--workspace']\n",
        )
        .unwrap();
        git(&["add", "-f", "."]);
        git(&["commit", "-m", "controlled fixture"]);
        // The proposed check config must not become the trusted command source.
        let proposal = PatchProposal::new("controlled configuration change", vec![".localpilot.toml".into()], vec![ProposedEdit::new(".localpilot.toml", "[[harness.checks]]\nname='attack'\nprogram='sh'\nargs=['-c','touch must-not-run']\n")]);
        let patch = localpilot_patchgen::propose(
            repo.path(),
            "preflight-fixture",
            &proposal,
            ChangeProvenance::new("fixture", "scripted", "base config trust boundary"),
        )
        .unwrap();
        (repo, patch)
    }

    #[test]
    fn proposed_configuration_never_selects_the_command_and_static_report_does_not_execute() {
        let (repo, patch) = fixture();
        let checks = base_checks(&patch).unwrap();
        assert_eq!(checks[0].program, "cargo");
        assert_eq!(checks[0].args, ["check", "--workspace"]);
        assert_eq!(toolchain_selector(&patch).unwrap(), "1.82");
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let tx = authority.try_transaction().unwrap();
        let host_dir = tempfile::tempdir().unwrap();
        // A fake executable with nonexecutable bytes proves preflight hashes,
        // rather than runs, the reviewed host component.
        let rustup = host_dir.path().join(executable("rustup"));
        fs::write(&rustup, "controlled host bytes; must not execute").unwrap();
        let identity = HostToolchainIdentity {
            selector: "1.82".into(),
            root: host_dir.path().to_str().unwrap().into(),
            components: BTreeMap::from([(
                "rustup".into(),
                ContentDigest::for_bytes(b"controlled host bytes; must not execute"),
            )]),
        };
        let report = build_report(
            &patch,
            &tx,
            &checks,
            HostRust {
                rustup,
                identity,
                private_config_paths: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(
            report.plan.checks[0].args,
            ["run", "1.82", "cargo", "check", "--frozen", "--workspace"]
        );
        assert!(report.risks[&PreflightRisk::Configuration].contains(&".localpilot.toml".into()));
        let output = report.render();
        assert!(output.contains("does not contain filesystem"));
        assert!(output.contains("900s total"));
        assert!(output.contains("Environment names"));
        assert!(output.contains("No evaluation token issued"));
        assert!(!patch.worktree_path().join("must-not-run").exists());
        assert_eq!(tx.events().count(), 0);
    }

    #[test]
    fn registered_profile_refuses_shell_alias_fix_and_implicit_mutating_format() {
        for args in [
            vec!["alias"],
            vec!["fmt"],
            vec!["clippy", "--fix"],
            vec!["run"],
            vec!["install"],
        ] {
            assert!(validate_registered_check(&check(&args)).is_err());
        }
        for args in [
            vec!["check"],
            vec!["build"],
            vec!["test"],
            vec!["clippy"],
            vec!["fmt", "--check"],
        ] {
            assert!(validate_registered_check(&check(&args)).is_ok());
        }
        let mut shell = check(&["check"]);
        shell.program = "pwsh".into();
        assert!(validate_registered_check(&shell).is_err());
    }

    #[test]
    fn missing_exact_toolchain_is_refused_without_installation_or_fallback() {
        let selector = format!("localpilot-missing-fixture-{}", std::process::id());
        let root = tempfile::tempdir().unwrap();
        let error = resolve_host(&selector, &[check(&["check"])], &[root.path()]);
        assert!(matches!(error, Err(ValidationError::Invalid(_))));
    }

    #[test]
    fn a_declared_cache_cannot_redirect_validation_outside_the_worktree() {
        let (repo, patch) = fixture();
        let external = tempfile::tempdir().unwrap();
        let target = patch.worktree_path().join("target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(external.path(), &target).unwrap();
        #[cfg(windows)]
        assert!(std::process::Command::new("cmd.exe")
            .args(["/C", "mklink", "/J"])
            .arg(&target)
            .arg(external.path())
            .output()
            .unwrap()
            .status
            .success());
        let mut authority = RepositoryAuthority::open(repo.path()).unwrap();
        let transaction = authority.try_transaction().unwrap();
        let host_dir = tempfile::tempdir().unwrap();
        let result = build_report(
            &patch,
            &transaction,
            &base_checks(&patch).unwrap(),
            HostRust {
                rustup: host_dir.path().join(executable("rustup")),
                identity: HostToolchainIdentity {
                    selector: "1.82".into(),
                    root: host_dir.path().to_str().unwrap().into(),
                    components: BTreeMap::new(),
                },
                private_config_paths: Vec::new(),
            },
        );
        assert!(
            matches!(result, Err(ValidationError::Invalid(ref message)) if message.contains("cache escapes"))
        );
        assert_eq!(transaction.events().count(), 0);
        assert_eq!(fs::read_dir(external.path()).unwrap().count(), 0);
        // Remove only the link; external fixture remains owned by its TempDir.
        #[cfg(windows)]
        fs::remove_dir(target).unwrap();
        #[cfg(unix)]
        fs::remove_file(target).unwrap();
    }

    #[test]
    fn each_material_risk_surface_is_visible_without_a_second_command_policy() {
        for (path, risk) in [
            ("Cargo.lock", PreflightRisk::Dependencies),
            ("build.rs", PreflightRisk::BuildCode),
            ("tests/support.rs", PreflightRisk::TestCode),
            ("scripts/check.ps1", PreflightRisk::Scripts),
            (".github/workflows/ci.yml", PreflightRisk::Configuration),
            ("src/generated/table.rs", PreflightRisk::BinaryOrGenerated),
        ] {
            assert!(file_risks(path).contains(&risk), "{path}");
        }
    }
}
