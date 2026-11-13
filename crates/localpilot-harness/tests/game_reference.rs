//! A Godot 4 game, run and observed through LocalPilot's own tools.
//!
//! A reference game (`tests/reference/godot-game`) falls onto a floor and moves
//! while the `move_right` action is held; its collision check has a defect
//! (an equality test) that lets the player fall through the floor. A session
//! with the real tools checks it at three levels, which this test keeps apart:
//!
//! 1. the project loads (`--headless --quit`): true for the broken game too;
//! 2. gameplay: a headless run presses `move_right` through the engine's input
//!    action pipeline and prints the player's state after 60 fixed frames;
//! 3. rendered state: a windowed run on the real GPU saves the viewport, the
//!    session reads the image with `read_image`, and the test reads the pixels
//!    itself with GDI+, independently of the game and of the session.
//!
//! The input is the engine's action pipeline, not operating-system key events.
//! `engine.run` stays a deferred route: this shows what the existing tools can
//! do with an installed engine, not that LocalPilot has an engine adapter.
//!
//! It needs Windows, a Godot 4 console build (set `LOCALPILOT_GODOT`, or have it
//! in `C:\Godot`, or `godot` on `PATH`) and an interactive desktop with OpenGL.
//! Otherwise it skips loudly; `LOCALPILOT_REQUIRE_REAL_TESTS=1` turns a skip into
//! a failure. A window opens for a few seconds, twice.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use localpilot_harness::{
    register_task_guidance_context, EvidenceState, GuidanceCatalog, GuidanceHookMode, ImageState,
    RouteStatus, RuntimeEvent, SessionConfig, SessionRuntime,
};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

const REQUIRE_VAR: &str = "LOCALPILOT_REQUIRE_REAL_TESTS";

fn skip(reason: &str) {
    assert!(
        std::env::var_os(REQUIRE_VAR).is_none(),
        "{REQUIRE_VAR} is set but the game test cannot run: {reason}"
    );
    eprintln!("SKIPPED (real Godot game): {reason}");
}

fn reference(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("reference")
        .join(name)
}

fn copy_project(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let path = entry.path();
        let target = to.join(entry.file_name());
        if path.is_dir() {
            copy_project(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

/// The Godot console executable, if one can be found.
fn godot() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("LOCALPILOT_GODOT") {
        return Some(PathBuf::from(explicit)).filter(|p| p.is_file());
    }
    if let Ok(entries) = std::fs::read_dir(r"C:\Godot") {
        let mut found: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("Godot_v4") && n.ends_with("_console.exe"))
            })
            .collect();
        found.sort();
        if let Some(path) = found.pop() {
            return Some(path);
        }
    }
    localpilot_harness::program_on_path("godot").then(|| PathBuf::from("godot"))
}

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

/// A pixel and a size, read with GDI+ and not by the game.
fn read_png(path: &Path, points: &[(u32, u32)]) -> (String, Vec<(u8, u8, u8)>) {
    let list = points
        .iter()
        .map(|(x, y)| format!("@({x},{y})"))
        .collect::<Vec<_>>()
        .join(",");
    let script = format!(
        "Add-Type -AssemblyName System.Drawing; $b=[System.Drawing.Bitmap]::new('{}'); \
         \"size=$($b.Width)x$($b.Height)\"; foreach ($p in @({list})) {{ $c=$b.GetPixel($p[0],$p[1]); \
         \"px=$($c.R),$($c.G),$($c.B)\" }}; $b.Dispose()",
        path.display()
    );
    let out = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let size = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("size="))
        .unwrap_or_default()
        .to_string();
    let pixels = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("px="))
        .map(|rgb| {
            let mut parts = rgb.split(',').map(|n| n.trim().parse::<u8>().unwrap());
            (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            )
        })
        .collect();
    (size, pixels)
}

fn state_in(output: &str) -> Option<Value> {
    output
        .lines()
        .find_map(|line| line.trim().strip_prefix("STATE "))
        .and_then(|json| serde_json::from_str(json).ok())
}

#[tokio::test]
async fn a_godot_game_is_loaded_played_and_rendered_through_the_tools() {
    if !cfg!(windows) {
        return skip("not Windows");
    }
    let Some(engine) = godot() else {
        return skip("no Godot 4 console build (set LOCALPILOT_GODOT, or put it in C:\\Godot)");
    };
    if !interactive_desktop() {
        return skip("no interactive desktop session to open a window in");
    }
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    copy_project(&reference("godot-game"), &project);
    let fixed = std::fs::read_to_string(reference("godot-game").join("fix/main.gd")).unwrap();
    let slash = |p: &Path| p.to_string_lossy().replace('\\', "/");
    let engine_path = engine.to_string_lossy().to_string();
    let shot_before = project.join("shot_before.png");
    let shot_after = project.join("shot_after.png");
    let headless = |tag: &str| {
        let _ = tag;
        json!({"program": engine_path, "args": ["--headless", "--path", slash(&project), "--fixed-fps", "60", "--quit-after", "600", "--", "--scenario", "run"]})
    };
    let rendered = |shot: &Path| json!({"program": engine_path, "args": ["--path", slash(&project), "--rendering-driver", "opengl3", "--resolution", "320x180", "--fixed-fps", "60", "--quit-after", "600", "--", "--scenario", "run", "--shot", slash(shot)]});
    let loads = json!({"program": engine_path, "args": ["--headless", "--path", slash(&project), "--quit"]});
    let provider = Arc::new(
        FakeProvider::new()
            .tool_call("load-1", "run_shell", loads)
            .tool_call("play-1", "run_shell", headless("before"))
            .tool_call("draw-1", "run_shell", rendered(&shot_before))
            .tool_call("look-1", "read_image", json!({"path": slash(&shot_before)}))
            .tool_call(
                "fix-1",
                "write_file",
                json!({"path": "main.gd", "content": fixed}),
            )
            .tool_call("play-2", "run_shell", headless("after"))
            .tool_call("draw-2", "run_shell", rendered(&shot_after))
            .tool_call("look-2", "read_image", json!({"path": slash(&shot_after)}))
            .text("The player now rests on the floor."),
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
    runtime.set_image_support_override(Some(true));
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let names = runtime.tool_names();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    register_task_guidance_context(
        Some(GuidanceHookMode::Assisted),
        GuidanceCatalog {
            root: project.clone(),
            home: Some(dir.path().join("home")),
            trusted: runtime.trust_flag(),
        },
        localpilot_harness::capabilities_from_tools(&names),
        &mut runtime,
    );
    let (events, mut rx) = broadcast::channel::<RuntimeEvent>(512);
    let outputs = Arc::new(Mutex::new(Vec::<(String, String)>::new()));
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
            "Fix the player falling through the floor in main.gd and show me the rendered scene.",
            &events,
            &CancellationToken::new(),
        )
        .await;
    drop(events);
    let _ = reader.await;
    let outputs = outputs.lock().unwrap().clone();
    let output_of = |id: &str| {
        outputs
            .iter()
            .find(|(call, _)| call.as_str() == id)
            .map(|(_, text)| text.clone())
            .unwrap_or_default()
    };
    for id in ["load-1", "play-1", "play-2", "draw-1", "draw-2"] {
        eprintln!("REAL GAME {id}: {}", output_of(id).replace('\n', " | "));
    }

    // 1. The project loads, broken or not: loading is not behaviour.
    assert!(
        output_of("load-1").contains("status: success"),
        "{}",
        output_of("load-1")
    );

    // 2. Gameplay through the engine's input action pipeline.
    let before = state_in(&output_of("play-1")).expect("no STATE line before the fix");
    let after = state_in(&output_of("play-2")).expect("no STATE line after the fix");
    assert_eq!(before["frames"], 60);
    assert!(
        before["y"].as_f64().unwrap() > 140.0,
        "the broken game must fall through the floor: {before}"
    );
    assert!(
        (before["x"].as_f64().unwrap() - 80.0).abs() < 0.05,
        "the held action moves the player: {before}"
    );
    assert!(
        (after["y"].as_f64().unwrap() - 108.0).abs() < 0.05,
        "the fixed player rests on the floor: {after}"
    );
    assert!(
        (after["x"].as_f64().unwrap() - 80.0).abs() < 0.05,
        "{after}"
    );

    // 3. Rendered state, read by the test itself.
    let points = [(96, 124), (10, 10), (10, 160)];
    let (size_before, px_before) = read_png(&shot_before, &points);
    let (size_after, px_after) = read_png(&shot_after, &points);
    eprintln!("REAL GAME before {size_before} {px_before:?}; after {size_after} {px_after:?}");
    assert_eq!(
        (size_before.as_str(), size_after.as_str()),
        ("320x180", "320x180")
    );
    // After the fix the red player is where the state says; before it, it is
    // below the visible area and the same spot shows the background.
    assert_eq!(px_after[0], (255, 0, 0), "{px_after:?}");
    assert_ne!(px_before[0], (255, 0, 0), "{px_before:?}");
    // The background and the floor are drawn in both.
    for pixels in [&px_before, &px_after] {
        assert_eq!(pixels[1], (12, 20, 51), "{pixels:?}");
        assert_eq!(pixels[2], (127, 127, 127), "{pixels:?}");
    }

    // The session's record keeps the levels apart.
    let observations = runtime.task_observations();
    let runs: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "process.capture")
        .collect();
    // Loading, two headless plays and two rendered plays.
    assert_eq!(runs.len(), 5, "{observations:?}");
    assert!(
        runs.iter().all(|o| o.state == EvidenceState::Passed),
        "{runs:?}"
    );
    let images: Vec<_> = observations
        .iter()
        .filter(|o| o.capability == "image.inspect")
        .collect();
    assert_eq!(images.len(), 2, "{images:?}");
    assert!(
        images
            .iter()
            .all(|o| o.image == Some(ImageState::Inspected)),
        "{images:?}"
    );

    // The engine, not .NET, was recognized, and its guidance chosen.
    let selection = runtime.guidance_selection().unwrap();
    let chosen = selection.selected_names();
    assert!(
        chosen.contains(&"guide-godot4") && chosen.contains(&"guide-game"),
        "{chosen:?}"
    );
    assert!(!chosen.contains(&"guide-dotnet"), "{chosen:?}");

    // The engine route is still deferred: no adapter was added.
    let route = runtime
        .observation_routes()
        .into_iter()
        .find(|r| r.capability == "engine.run")
        .unwrap();
    assert!(
        matches!(&route.status, RouteStatus::Deferred(why) if why.contains("headless engine command is process evidence only")),
        "{route:?}"
    );
    let requested = runtime.requested_evidence();
    let engine_claim = requested.iter().find(|r| r.capability == "engine.run");
    assert_eq!(
        engine_claim.map(|r| r.state),
        Some(EvidenceState::Unavailable),
        "{requested:?}"
    );
    assert!(
        std::fs::read_to_string(project.join("main.gd"))
            .unwrap()
            .contains(">= FLOOR_Y"),
        "the fix was written by the session"
    );
}
