//! Qualify the Rust CLI and .NET service workflows against reference projects.
//!
//! Each reference project (under `tests/reference/`) starts with a known
//! defect. A real session, with the built-in guidance in assisted mode and a
//! scripted provider, builds it and later applies the fix through LocalPilot's
//! ordinary tools (`run_shell`, `write_file`), with permission checks in force.
//! A grader written here, independent of the session, then exercises the
//! result: the CLI with valid, invalid and held-out arguments; the service on
//! an isolated local port with valid and invalid requests and the state they
//! leave. A build that succeeds while behaviour fails is reported as exactly
//! that.
//!
//! The Rust project has no dependencies and builds offline with the pinned
//! toolchain. The .NET project pins its SDK major in `global.json`, clears all
//! package sources in `NuGet.config` so restore cannot reach a network, and
//! needs no package; its test is skipped, with a message, when no `dotnet` is
//! on `PATH`. Neither touches a production service or an external database.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write as _};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use localpilot_harness::{
    register_task_guidance_context, EvidenceState, GuidanceCatalog, GuidanceHookMode, RuntimeEvent,
    SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

fn reference(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("reference")
        .join(name)
}

/// Copy a reference project, dropping the `.fixture` suffix that keeps its
/// manifest out of this repository's own tooling.
fn copy_project(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let target = to.join(name.strip_suffix(".fixture").unwrap_or(&name));
        if path.is_dir() {
            copy_project(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn sha256(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn session(dir: &Path, provider: Arc<FakeProvider>) -> SessionRuntime {
    let project = dir.join("project");
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&dir.join("store")),
        Workspace::new(&project).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let names = runtime.tool_names();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let capabilities = localpilot_harness::capabilities_from_tools(&names);
    let catalog = GuidanceCatalog {
        root: project,
        home: Some(home),
        trusted: runtime.trust_flag(),
    };
    register_task_guidance_context(
        Some(GuidanceHookMode::Assisted),
        catalog,
        capabilities,
        &mut runtime,
    );
    runtime
}

async fn turn(runtime: &mut SessionRuntime, prompt: &str) {
    let (events, _rx) = broadcast::channel::<RuntimeEvent>(512);
    runtime
        .run_turn(prompt, &events, &CancellationToken::new())
        .await;
}

/// The process observations of a session, as `(call, state, result)`.
fn process_runs(runtime: &SessionRuntime) -> Vec<(String, EvidenceState, String)> {
    runtime
        .task_observations()
        .into_iter()
        .filter(|o| o.capability == "process.capture")
        .map(|o| (o.call, o.state, o.result))
        .collect()
}

/// One graded check: what was run, what was expected, what happened.
#[derive(Debug)]
struct Check {
    case: String,
    expected: String,
    actual: String,
    passed: bool,
}

fn summarize(checks: &[Check]) -> String {
    checks
        .iter()
        .map(|c| {
            format!(
                "  [{}] {}: expected {}, got {}",
                if c.passed { "pass" } else { "FAIL" },
                c.case,
                c.expected,
                c.actual
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------- V1: Rust CLI

/// Grade the built `counter`: valid output, usage errors for invalid input,
/// and held-out inputs that were never part of the fix.
fn grade_counter(binary: &Path) -> Vec<Check> {
    let mut checks = Vec::new();
    let mut run = |label: &str, args: &[&str], code: i32, stdout: &str, stderr_has: &str| {
        let output = Command::new(binary).args(args).output().unwrap();
        let actual_code = output.status.code().unwrap_or(-1);
        let out = String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n");
        let err = String::from_utf8_lossy(&output.stderr).to_lowercase();
        let passed = actual_code == code && out == stdout && err.contains(stderr_has);
        checks.push(Check {
            case: label.to_string(),
            expected: format!("exit {code}, stdout {stdout:?}, stderr containing {stderr_has:?}"),
            actual: format!("exit {actual_code}, stdout {out:?}, stderr {err:?}"),
            passed,
        });
    };
    run("valid --count 3", &["--count", "3"], 0, "1\n2\n3\n", "");
    run(
        "negative --count -1",
        &["--count", "-1"],
        2,
        "",
        "invalid count",
    );
    run(
        "malformed --count abc",
        &["--count", "abc"],
        2,
        "",
        "invalid count",
    );
    run("missing value", &["--count"], 2, "", "usage");
    // Held out: not named in the task.
    run("held-out --count 0", &["--count", "0"], 0, "", "");
    run(
        "held-out --count 1.5",
        &["--count", "1.5"],
        2,
        "",
        "invalid count",
    );
    checks
}

#[tokio::test]
async fn the_rust_cli_workflow_builds_fixes_and_passes_an_independent_grader() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    copy_project(&reference("rust-cli"), &project);
    let readme = sha256(&project.join("README.md"));
    let fixed = std::fs::read_to_string(reference("rust-cli.fix").join("src/main.rs")).unwrap();
    let build = json!({"program": "cargo", "args": ["build", "--offline", "--quiet", "--target-dir", "target"]});
    let binary = project.join("target").join("debug").join(if cfg!(windows) {
        "counter.exe"
    } else {
        "counter"
    });
    // The session also runs the built program itself, so the behaviour is
    // observed through the tools and not only by the grader.
    let invalid = json!({"program": binary.to_string_lossy(), "args": ["--count", "-1"]});
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("build-1", "run_shell", build.clone())
            .tool_call("run-1", "run_shell", invalid.clone())
            .text("Built; --count -1 exits 0.")
            .tool_call(
                "fix-1",
                "write_file",
                json!({"path": "src/main.rs", "content": fixed}),
            )
            .tool_call("build-2", "run_shell", build)
            .tool_call("run-2", "run_shell", invalid)
            .text("Fixed."),
    );
    let mut runtime = session(dir.path(), provider);

    turn(
        &mut runtime,
        "Zoek uit waarom --count -1 met exitcode 0 eindigt.",
    )
    .await;
    let runs = process_runs(&runtime);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(
        runs[0].1,
        EvidenceState::Passed,
        "the defective project must build: {runs:?}"
    );
    // Observed through the tools: the invalid value exits 0, which is the defect.
    assert_eq!(runs[1].2, "exit 0", "{runs:?}");
    let before = grade_counter(&binary);
    eprintln!("V1 before the fix (build passed):\n{}", summarize(&before));
    // Build success is not behaviour success, and the failures are kept.
    assert!(before.iter().any(|c| !c.passed));
    assert!(
        before
            .iter()
            .find(|c| c.case.starts_with("valid"))
            .unwrap()
            .passed
    );

    turn(
        &mut runtime,
        "Los het op: ongeldige waarden moeten exitcode 2 geven.",
    )
    .await;
    let runs = process_runs(&runtime);
    assert_eq!(runs.len(), 4, "{runs:?}");
    assert_eq!(runs[2].1, EvidenceState::Passed, "{runs:?}");
    // After the fix the same call exits 2. The record reports that as a
    // failed command: it says what the tool observed, and the grader below
    // decides that exit 2 is the correct behaviour.
    assert_eq!(
        (runs[3].1, runs[3].2.as_str()),
        (EvidenceState::Failed, "exit 2"),
        "{runs:?}"
    );
    let after = grade_counter(&binary);
    eprintln!("V1 after the fix:\n{}", summarize(&after));
    assert!(after.iter().all(|c| c.passed), "{}", summarize(&after));
    assert_eq!(
        sha256(&project.join("README.md")),
        readme,
        "unrelated file changed"
    );

    let selection = runtime.guidance_selection().unwrap();
    assert!(selection.selected_names().contains(&"guide-cli"));
    let toolchain = Command::new("rustc").arg("--version").output().unwrap();
    eprintln!(
        "V1 target counter (bin), toolchain {}, command cargo build --offline",
        String::from_utf8_lossy(&toolchain.stdout).trim()
    );
}

// ------------------------------------------------------------ V2: .NET service

/// A child process that is killed when dropped, so a failed assertion never
/// leaves the service running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One HTTP/1.1 request to the isolated instance: status and body.
fn http(port: u16, method: &str, path: &str, body: Option<&str>) -> Option<(u16, String)> {
    let address: SocketAddr = format!("127.0.0.1:{port}").parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .ok()?;
    let body = body.unwrap_or("");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut response = String::new();
    stream.read_to_string(&mut response).ok()?;
    let status = response.split_whitespace().nth(1)?.parse().ok()?;
    let (head, raw) = response
        .split_once("\r\n\r\n")
        .unwrap_or((response.as_str(), ""));
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(raw)
    } else {
        raw.to_string()
    };
    Some((status, body))
}

/// The body of a `Transfer-Encoding: chunked` response.
fn dechunk(mut raw: &str) -> String {
    let mut body = String::new();
    while let Some((size, rest)) = raw.split_once("\r\n") {
        let Ok(size) = usize::from_str_radix(size.trim(), 16) else {
            break;
        };
        if size == 0 || rest.len() < size {
            break;
        }
        body.push_str(&rest[..size]);
        raw = rest[size..].trim_start_matches("\r\n");
    }
    body
}

/// Start the built service on a free local port and wait for real readiness.
fn start_service(project: &Path) -> (Running, u16) {
    let port = free_port();
    let dll = project
        .join("bin")
        .join("Debug")
        .join("net10.0")
        .join("Api.dll");
    let child = Command::new("dotnet")
        .arg(&dll)
        .arg("--urls")
        .arg(format!("http://127.0.0.1:{port}"))
        .current_dir(project)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running(child);
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if matches!(http(port, "GET", "/health", None), Some((200, _))) {
            return (running, port);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    panic!("the service did not become ready on port {port}");
}

/// Grade the service: a valid item is created and retrievable; invalid items
/// are refused with 400 and leave no trace.
fn grade_service(project: &Path) -> Vec<Check> {
    let (_running, port) = start_service(project);
    let mut checks = Vec::new();
    let mut post = |label: &str, body: &str, want: u16| {
        let got = http(port, "POST", "/items", Some(body)).map_or(0, |(status, _)| status);
        checks.push(Check {
            case: label.to_string(),
            expected: format!("status {want}"),
            actual: format!("status {got}"),
            passed: got == want,
        });
    };
    post("valid item", r#"{"name": "pen", "quantity": 2}"#, 201);
    post("empty name", r#"{"name": "", "quantity": 2}"#, 400);
    post("zero quantity", r#"{"name": "cup", "quantity": 0}"#, 400);
    // Held out: not named in the task.
    post(
        "held-out negative quantity",
        r#"{"name": "mug", "quantity": -3}"#,
        400,
    );
    let stored = http(port, "GET", "/items", None)
        .map(|(_, body)| body)
        .unwrap_or_default();
    let names: Vec<String> = serde_json::from_str::<serde_json::Value>(&stored)
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|item| item["name"].as_str().map(str::to_string))
        .collect();
    checks.push(Check {
        case: "state after the requests".to_string(),
        expected: "only [\"pen\"] stored".to_string(),
        actual: format!("{names:?} stored"),
        passed: names == ["pen"],
    });
    checks
}

#[tokio::test]
async fn the_dotnet_service_workflow_builds_fixes_and_passes_an_independent_grader() {
    if !localpilot_harness::program_on_path("dotnet") {
        eprintln!(
            "skipped: no dotnet SDK on PATH; the .NET service workflow is not qualified here"
        );
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    copy_project(&reference("dotnet-api"), &project);
    let fixed = std::fs::read_to_string(reference("dotnet-api.fix").join("Program.cs")).unwrap();
    let build = json!({"program": "dotnet", "args": ["build", "--nologo", "-v", "q", "-nodeReuse:false", "-p:UseSharedCompilation=false"]});
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("build-1", "run_shell", build.clone())
            .text("Built; invalid items are accepted.")
            .tool_call(
                "fix-1",
                "write_file",
                json!({"path": "Program.cs", "content": fixed}),
            )
            .tool_call("build-2", "run_shell", build)
            .text("Fixed."),
    );
    let mut runtime = session(dir.path(), provider);

    turn(
        &mut runtime,
        "Investigate why the API accepts an item without a name.",
    )
    .await;
    let runs = process_runs(&runtime);
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        runs[0].1,
        EvidenceState::Passed,
        "the defective service must build: {runs:?}"
    );
    let before = grade_service(&project);
    eprintln!("V2 before the fix (build passed):\n{}", summarize(&before));
    assert!(before.iter().any(|c| !c.passed));
    assert!(
        before
            .iter()
            .find(|c| c.case == "valid item")
            .unwrap()
            .passed
    );

    turn(
        &mut runtime,
        "Fix it: invalid items must get 400 and not be stored.",
    )
    .await;
    let runs = process_runs(&runtime);
    assert_eq!(runs.len(), 2, "{runs:?}");
    assert_eq!(runs[1].1, EvidenceState::Passed, "{runs:?}");
    let after = grade_service(&project);
    eprintln!("V2 after the fix:\n{}", summarize(&after));
    assert!(after.iter().all(|c| c.passed), "{}", summarize(&after));

    let selection = runtime.guidance_selection().unwrap();
    assert!(selection.selected_names().contains(&"guide-service"));
    let sdk = Command::new("dotnet").arg("--version").output().unwrap();
    eprintln!(
        "V2 target Api (net10.0), SDK {}, command dotnet build, instance on an isolated local port",
        String::from_utf8_lossy(&sdk.stdout).trim()
    );
}
