//! A native desktop window, observed through LocalPilot's own tools.
//!
//! A WPF reference app (`tests/reference/wpf-app`) is built with `run_shell`,
//! started with `run_background`, observed and driven with `run_shell` running
//! a Windows UI Automation script, and stopped with `run_background`. The
//! script reads the window's status, presses its Save button twice through the
//! Invoke pattern and reads the status again, so the result is the native
//! window's own state, not a unit or headless test.
//!
//! This qualifies what the existing tools can do for a desktop app. It does not
//! add a desktop adapter: `desktop.interact` stays deferred, because a recipe
//! that works for one framework through a script is not a tested route for
//! windows in general. The test says so.
//!
//! It needs Windows, an interactive desktop session and a .NET SDK. Otherwise it
//! skips loudly; `LOCALPILOT_REQUIRE_REAL_TESTS=1` turns a skip into a failure.
//! The window opens on the desktop for a moment.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use localpilot_harness::{EvidenceState, RouteStatus, RuntimeEvent, SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::json;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const REQUIRE_VAR: &str = "LOCALPILOT_REQUIRE_REAL_TESTS";

fn skip(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_VAR).is_none(),
        "{REQUIRE_VAR} is set but the desktop test cannot run: {reason}"
    );
    eprintln!("SKIPPED (real desktop window): {reason}");
}

fn reference(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("reference")
        .join(name)
}

/// Copy a reference project, dropping the `.fixture` suffix.
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

fn on_path(program: &str) -> bool {
    localpilot_harness::program_on_path(program)
}

/// Whether this process can see an interactive desktop.
fn interactive_desktop() -> bool {
    Command::new("powershell")
        .args(["-NoProfile", "-Command", "[Environment]::UserInteractive"])
        .output()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .trim()
                .eq_ignore_ascii_case("true")
        })
        .unwrap_or(false)
}

/// Kill exactly this process (and its children). Never by name.
struct KillOnDrop(Option<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
    }
}

#[tokio::test]
async fn a_native_wpf_window_is_started_driven_and_stopped_through_the_tools() {
    if !cfg!(windows) {
        return skip("not Windows; WPF does not run here");
    }
    if !on_path("dotnet") {
        return skip("no dotnet SDK on PATH");
    }
    if !on_path("powershell") || !interactive_desktop() {
        return skip("no interactive desktop session to open a window in");
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    copy_project(&reference("wpf-app"), &project);
    let exe = project.join("bin/Debug/net10.0-windows/Notes.exe");
    let script = project.join("observe.ps1");

    let build = json!({"program": "dotnet", "args": ["build", "--nologo", "-v", "q", "-nodeReuse:false", "-p:UseSharedCompilation=false"]});
    // `args` must be non-empty: with none, `run_background` treats `program` as a
    // shell command line, and the shell does not wait for a GUI program, so the
    // tool reports "exited within 2s" while the window stays open untracked.
    let start = json!({"action": "start", "program": exe.to_string_lossy(), "args": ["--reference"], "grace_secs": 2});
    let observe = json!({"program": "powershell", "args": ["-NoProfile", "-ExecutionPolicy", "Bypass", "-File", script.to_string_lossy()]});
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("build-1", "run_shell", build)
            .tool_call("start-1", "run_background", start)
            .tool_call("observe-1", "run_shell", observe)
            .tool_call(
                "stop-1",
                "run_background",
                json!({"action": "stop", "id": "bg-1"}),
            )
            .text("Built, opened the window, pressed Save twice and closed it."),
    );
    let mut runtime = SessionRuntime::new(
        provider as Arc<dyn localpilot_llm::ModelProvider>,
        ToolRegistry::with_builtins(),
        PermissionEngine::new(Profile::Default, Vec::new()),
        Box::new(ScriptedApprover::always()),
        Store::open(&dir.path().join("store")),
        Workspace::new(&project).unwrap(),
        RecoveryEngine::new(RecoveryBudget::default()),
        SessionConfig::default(),
        Vec::new(),
    );
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(512);
    // The output of each tool call, as the session reported it.
    let outputs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, String)>::new()));
    let collected = Arc::clone(&outputs);
    let reader = tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if let RuntimeEvent::ToolFinished { id, output, .. } = event {
                collected.lock().unwrap().push((id, output));
            }
        }
    });
    runtime
        .run_turn(
            "Build the Notes app, open it, press Save twice and check the status text.",
            &events,
            &CancellationToken::new(),
        )
        .await;
    drop(events);
    let _ = reader.await;
    let outputs = outputs.lock().unwrap().clone();
    let output_of = |id: &str| -> String {
        outputs
            .iter()
            .find(|(call, _)| call.as_str() == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    };

    let observed = output_of("observe-1");
    eprintln!("REAL DESKTOP observe output:\n{observed}");
    eprintln!("REAL DESKTOP start output:\n{}", output_of("start-1"));
    eprintln!("REAL DESKTOP stop output:\n{}", output_of("stop-1"));
    // Make sure the window is gone whatever happens below.
    let pid = observed
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid="))
        .and_then(|pid| pid.trim().parse::<u32>().ok());
    let _guard = KillOnDrop(pid);
    if observed.contains("NOWINDOW") {
        return skip("the window never appeared on a desktop this process can see");
    }

    assert!(
        exe.is_file(),
        "the build did not produce {exe:?}: {}",
        output_of("build-1")
    );
    // The native window's own state, read and changed through UI Automation.
    assert!(observed.contains("title=Notes reference"), "{observed}");
    assert!(observed.contains("before=Not saved"), "{observed}");
    assert!(observed.contains("after=Saved 2"), "{observed}");
    let hwnd = observed
        .lines()
        .find_map(|line| line.trim().strip_prefix("hwnd="))
        .and_then(|h| h.trim().parse::<i64>().ok());
    assert!(
        hwnd.is_some_and(|h| h > 0),
        "no native window handle: {observed}"
    );

    // The record: a build, a background start, a native observation, a stop.
    let observations = runtime.task_observations();
    let process: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "process.capture")
        .collect();
    assert_eq!(process.len(), 2, "{observations:?}");
    assert!(
        process.iter().all(|o| o.state == EvidenceState::Passed),
        "{process:?}"
    );
    assert!(
        observations
            .iter()
            .any(|o| o.capability == "process.background" && o.state == EvidenceState::Passed),
        "{observations:?}"
    );

    // The window is gone after the stop: nothing named Notes still runs.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let still_running = Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Name Notes -ErrorAction SilentlyContinue | Measure-Object).Count",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    assert_eq!(still_running, "0", "the window was not closed by the stop");

    // What this does and does not qualify: the route stays deferred.
    let route = runtime
        .observation_routes()
        .into_iter()
        .find(|r| r.capability == "desktop.interact")
        .unwrap();
    assert!(
        matches!(&route.status, RouteStatus::Deferred(why) if why.contains("headless tests are not window interaction")),
        "{route:?}"
    );
}
