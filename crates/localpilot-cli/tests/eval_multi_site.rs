//! A headless evaluation renaming one function across several sites.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use assert_cmd::Command;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn sse(chunks: &[Value]) -> ResponseTemplate {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn calls_tool(id: usize, name: &str, args: &Value) -> ResponseTemplate {
    sse(&[
        json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": format!("call_{id}"),
            "function": {"name": name, "arguments": args.to_string()}}]}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

fn says(text: &str) -> ResponseTemplate {
    sse(&[json!({"choices": [{"delta": {"content": text}}]})])
}

/// One edit per rename site, in order, then a final answer.
struct RenameSites {
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<Value>>>,
}

const SITES: [(&str, &str, &str); 4] = [
    ("money.py", "def old_name(amount):", "def new_name(amount):"),
    (
        "money.py",
        "    return old_name(amount) * 2",
        "    return new_name(amount) * 2",
    ),
    (
        "app.py",
        "from money import old_name",
        "from money import new_name",
    ),
    ("app.py", "    print(old_name(3))", "    print(new_name(3))"),
];

impl Respond for RenameSites {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap_or(Value::Null));
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        match SITES.get(n) {
            Some((path, old, new)) => calls_tool(
                n,
                "edit_file",
                &json!({"path": path, "old_text": old, "new_text": new}),
            ),
            None => says("Renamed old_name to new_name at every site."),
        }
    }
}

fn git(root: &Path, args: &[&str]) {
    assert!(std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .status()
        .unwrap()
        .success());
}

fn fixture(root: &Path, config: &Path, server: &str) {
    std::fs::create_dir_all(config.join("localpilot")).unwrap();
    std::fs::write(
        config.join("localpilot/config.toml"),
        "[[permissions.allow_commands]]
program = \"python\"
args_prefix = [\"-m\", \"unittest\", \"discover\", \"-s\", \"tests\"]
",
    )
    .unwrap();
    std::fs::write(
        root.join(".localpilot.toml"),
        format!("[provider]\ndefault = \"local\"\n[providers.local]\nkind = \"openai-compatible\"\nbase_url = \"{server}\"\n"),
    )
    .unwrap();
    std::fs::create_dir(root.join(".localmind")).unwrap();
    std::fs::write(
        root.join(".localmind.toml"),
        "[learning]\nenabled = false\n",
    )
    .unwrap();
    std::fs::write(
        root.join("money.py"),
        "def old_name(amount):\n    return round(amount, 2)\n\n\ndef doubled(amount):\n    return old_name(amount) * 2\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app.py"),
        "from money import old_name\n\n\ndef main():\n    print(old_name(3))\n",
    )
    .unwrap();
    std::fs::create_dir(root.join("tests")).unwrap();
    std::fs::write(
        root.join("tests/test_rename.py"),
        "import contextlib\nimport io\nimport sys\nimport unittest\nsys.path.insert(0, '.')\n\n\nclass Rename(unittest.TestCase):\n    def test_every_site_uses_new_name(self):\n        import app\n        import money\n        self.assertFalse(hasattr(money, 'old_name'))\n        self.assertEqual(money.doubled(1.234), 2.46)\n        out = io.StringIO()\n        with contextlib.redirect_stdout(out):\n            app.main()\n        self.assertEqual(out.getvalue(), '3\\n')\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), ".localpilot/\n.localmind/\n").unwrap();
    git(root, &["init", "-q"]);
    git(root, &["add", "-A"]);
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
}

#[allow(dead_code)]
fn tool_results(requests: &[Value]) -> Vec<String> {
    let Some(last) = requests.last() else {
        return Vec::new();
    };
    last["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].to_string())
        .collect()
}

const VERIFY: &str = "python -m unittest discover -s tests";

async fn run_rename(
    verify: bool,
) -> (std::process::Output, PathBuf, Vec<Value>, tempfile::TempDir) {
    let server = MockServer::start().await;
    // A llama.cpp-style server attests a 16k served window.
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"default_generation_settings": {"n_ctx": 16384}})),
        )
        .mount(&server)
        .await;
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(RenameSites {
            calls: AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        })
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fixture(root.path(), config.path(), &server.uri());
    let workspace = root.path().to_path_buf();
    let user_config = config.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        let mut args = vec!["eval", "--model", "fixture", "--permission", "bypass"];
        if verify {
            args.extend(["--verify-command", VERIFY]);
        } else {
            args.push("--no-verify");
        }
        args.push("Rename old_name to new_name everywhere.");
        Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .current_dir(workspace)
            .env("APPDATA", &user_config)
            .env("XDG_CONFIG_HOME", &user_config)
            .args(args)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let recorded = requests.lock().unwrap().clone();
    let path = root.path().to_path_buf();
    drop(config);
    (output, path, recorded, root)
}

fn report(label: &str, output: &std::process::Output, root: &Path, requests: &[Value]) {
    use sha2::Digest;
    let exe = std::fs::read(env!("CARGO_BIN_EXE_localpilot")).unwrap();
    println!("=== {label}");
    println!("executable sha256: {:x}", sha2::Sha256::digest(&exe));
    println!("status: {:?}", output.status.code());
    println!(
        "stdout:
{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "stderr:
{}",
        String::from_utf8_lossy(&output.stderr)
    );
    println!("provider requests: {}", requests.len());
    if let Some(first) = requests.first() {
        for m in first["messages"].as_array().into_iter().flatten() {
            if m["role"] == "user" {
                println!("first user message: {}", m["content"]);
            }
        }
    }
    for m in requests
        .last()
        .map(|r| r["messages"].clone())
        .unwrap_or(Value::Null)
        .as_array()
        .into_iter()
        .flatten()
    {
        if m["role"] != "system" {
            println!(
                "[{}] {}",
                m["role"],
                m["content"]
                    .to_string()
                    .chars()
                    .take(600)
                    .collect::<String>()
            );
        }
    }
    for file in ["money.py", "app.py"] {
        println!(
            "--- {file}
{}",
            std::fs::read_to_string(root.join(file)).unwrap()
        );
    }
    let mut stack = vec![root.join(".localpilot")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|e| e == "jsonl") {
                for line in std::fs::read_to_string(&p).unwrap_or_default().lines() {
                    if line.contains("Check")
                        || line.contains("Verif")
                        || line.contains("Preflight")
                    {
                        println!("event: {}", line.chars().take(400).collect::<String>());
                    }
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "reproduction of the multi-site envelope stop; prints evidence"]
async fn reproduce_multi_site_rename_with_verification() {
    let (output, root, requests, _keep) = run_rename(true).await;
    report("verify-command", &output, &root, &requests);
}

#[tokio::test]
#[ignore = "control: the same reproduction without the optional automatic check"]
async fn reproduce_multi_site_rename_without_optional_verification() {
    let (output, root, requests, _keep) = run_rename(false).await;
    report("no-verify", &output, &root, &requests);
}

/// The fixture's checker must fail for every partial rename and pass only
/// when all four sites are renamed.
fn checker_after(sites: usize) -> Option<bool> {
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fixture(root.path(), config.path(), "http://127.0.0.1:9");
    for (file, old, new) in &SITES[..sites] {
        let p = root.path().join(file);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains(old), "{file}: {old}");
        std::fs::write(&p, text.replacen(old, new, 1)).unwrap();
    }
    let status = std::process::Command::new("python")
        .args(["-m", "unittest", "discover", "-s", "tests"])
        .current_dir(root.path())
        .output()
        .ok()?
        .status;
    Some(status.success())
}

#[test]
#[ignore = "checker control for the reproduction fixture; needs python"]
fn fixture_checker_passes_only_after_every_site() {
    for sites in 0..=SITES.len() {
        let Some(passed) = checker_after(sites) else {
            return;
        };
        println!("sites renamed: {sites} -> checker passed: {passed}");
        assert_eq!(passed, sites == SITES.len(), "sites renamed: {sites}");
    }
}

/// How the scripted model behaves once it has applied what it can.
#[derive(Clone, Copy, PartialEq)]
enum Plan {
    /// Rename the next remaining site; end the unit when the envelope refuses.
    Rename,
    /// Rename the first site, then answer "done" without further edits.
    StopAfterFirst,
    /// Rename the first site, run a shell command, then end the unit.
    ShellInUnit,
}

/// A model that reads the workspace to choose its next site, like a model that
/// inspects files before editing, and ends a unit when the envelope refuses.
struct Workspace {
    root: PathBuf,
    plan: Plan,
    calls: AtomicUsize,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Respond for Workspace {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
        self.requests.lock().unwrap().push(body.clone());
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let last = body["messages"]
            .as_array()
            .and_then(|m| m.last())
            .cloned()
            .unwrap_or(Value::Null);
        let last_text = last["content"].to_string();
        if last["role"] == "tool" && last_text.contains("work envelope") {
            return says("This unit is done; the remaining old_name sites still need renaming.");
        }
        let remaining = SITES.iter().find(|(file, old, _)| {
            std::fs::read_to_string(self.root.join(file))
                .unwrap_or_default()
                .contains(old)
        });
        let renamed_any = !std::fs::read_to_string(self.root.join("money.py"))
            .unwrap_or_default()
            .contains("def old_name");
        match (self.plan, remaining) {
            (Plan::StopAfterFirst, _) if renamed_any => says("Done."),
            (Plan::ShellInUnit, _)
                if renamed_any && last["role"] == "tool" && last_text.contains("edited") =>
            {
                calls_tool(
                    n,
                    "run_shell",
                    &json!({"program": "git", "args": ["status"]}),
                )
            }
            (_, Some((file, old, new))) => calls_tool(
                n,
                "edit_file",
                &json!({"path": file, "old_text": old, "new_text": new}),
            ),
            (_, None) => says("Renamed old_name to new_name at every site."),
        }
    }
}

struct Run {
    output: std::process::Output,
    card: Value,
    requests: Vec<Value>,
    root: tempfile::TempDir,
}

impl Run {
    fn exit_reason(&self) -> &str {
        self.card["process"]["exit_reason"].as_str().unwrap_or("")
    }

    fn sites_renamed(&self) -> usize {
        SITES
            .iter()
            .filter(|(file, _, new)| {
                std::fs::read_to_string(self.root.path().join(file))
                    .unwrap_or_default()
                    .contains(new)
            })
            .count()
    }

    fn turn_stops(&self) -> Vec<String> {
        let dir = self.root.path().join(".localpilot/sessions");
        let mut stops = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            for line in std::fs::read_to_string(entry.path())
                .unwrap_or_default()
                .lines()
            {
                let Ok(event) = serde_json::from_str::<Value>(line) else {
                    continue;
                };
                if let Some(stop) = find_turn_stop(&event) {
                    stops.push(stop);
                }
            }
        }
        stops
    }
}

/// The `stop` label of a `TurnEnded` event, wherever the log nests it.
fn find_turn_stop(event: &Value) -> Option<String> {
    match event {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("turn_ended") {
                return map.get("stop").and_then(Value::as_str).map(str::to_string);
            }
            map.values().find_map(find_turn_stop)
        }
        _ => None,
    }
}

async fn run_eval(plan: Plan, units: Option<&str>, verify: bool, grant_verifier: bool) -> Run {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/props"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"default_generation_settings": {"n_ctx": 16384}})),
        )
        .mount(&server)
        .await;
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fixture(root.path(), config.path(), &server.uri());
    if !grant_verifier {
        std::fs::write(config.path().join("localpilot/config.toml"), "").unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(Workspace {
            root: root.path().to_path_buf(),
            plan,
            calls: AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        })
        .mount(&server)
        .await;
    let workspace = root.path().to_path_buf();
    let user_config = config.path().to_path_buf();
    let units = units.map(str::to_string);
    let output = tokio::task::spawn_blocking(move || {
        let permission = if grant_verifier { "bypass" } else { "default" };
        let mut args = vec!["eval", "--model", "fixture", "--permission", permission];
        if verify {
            args.extend(["--verify-command", VERIFY]);
        } else {
            args.push("--no-verify");
        }
        args.push("Rename old_name to new_name everywhere.");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_localpilot"));
        // Same-length renames inside one second can leave bytecode that
        // Python still treats as current; never let a stale cache decide.
        cmd.current_dir(workspace)
            .env("APPDATA", &user_config)
            .env("XDG_CONFIG_HOME", &user_config)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args(args);
        if let Some(units) = units {
            cmd.env("LOCALPILOT_HARNESS__GRANULARITY__HEADLESS_UNITS", units);
        }
        cmd.output().unwrap()
    })
    .await
    .unwrap();
    let card = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .unwrap_or(Value::Null);
    let requests = requests.lock().unwrap().clone();
    drop(config);
    Run {
        output,
        card,
        requests,
        root,
    }
}

fn python_available() -> bool {
    std::process::Command::new("python")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn text(run: &Run) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}\nstops: {:?}",
        String::from_utf8_lossy(&run.output.stdout),
        String::from_utf8_lossy(&run.output.stderr),
        run.turn_stops()
    )
}

fn all_messages(run: &Run) -> String {
    run.requests
        .iter()
        .map(|r| r["messages"].to_string())
        .collect()
}

#[tokio::test]
async fn units_finish_a_multi_site_rename_only_through_a_passing_full_check() {
    if !python_available() {
        return;
    }
    for verify in [true, false] {
        let run = run_eval(Plan::Rename, Some("8"), verify, true).await;
        assert_eq!(run.sites_renamed(), 4, "verify={verify} {}", text(&run));
        assert_eq!(run.exit_reason(), "Done", "verify={verify} {}", text(&run));
        let stops = run.turn_stops();
        assert!(
            stops.iter().filter(|s| *s == "UnitCheckpoint").count() >= 3,
            "verify={verify} {stops:?}"
        );
        assert_eq!(stops.last().map(String::as_str), Some("Done"));
        let cards = String::from_utf8_lossy(&run.output.stdout)
            .lines()
            .filter(|line| line.starts_with("{\"schema\""))
            .count();
        assert_eq!(
            cards,
            1,
            "one scorecard after the final unit: {}",
            text(&run)
        );
        let all = all_messages(&run);
        assert!(all.contains("Continue the same task with the next bounded unit"));
        assert!(all.contains("this unit is complete"), "{all}");
    }
}

#[tokio::test]
async fn without_the_setting_the_task_stops_as_before() {
    if !python_available() {
        return;
    }
    for units in [None, Some("0")] {
        let run = run_eval(Plan::Rename, units, true, true).await;
        assert_eq!(run.sites_renamed(), 1, "{}", text(&run));
        assert_eq!(run.exit_reason(), "NoProgress", "{}", text(&run));
        assert!(!run.turn_stops().iter().any(|s| s == "UnitCheckpoint"));
        let all = all_messages(&run);
        assert!(all.contains("verify it, and checkpoint before continuing"));
        assert!(!all.contains("this unit is complete"));
        assert!(!all.contains("next bounded unit"));
    }
}

#[tokio::test]
async fn a_checkpoint_followed_by_no_edits_never_finishes() {
    if !python_available() {
        return;
    }
    for verify in [true, false] {
        let run = run_eval(Plan::StopAfterFirst, Some("8"), verify, true).await;
        let stops = run.turn_stops();
        assert_eq!(
            stops.first().map(String::as_str),
            Some("UnitCheckpoint"),
            "{}",
            text(&run)
        );
        assert_ne!(run.exit_reason(), "Done", "verify={verify} {}", text(&run));
        assert_eq!(run.card["results"]["passed"], false);
        assert!(stops.iter().all(|s| s != "Done"), "{stops:?}");
    }
}

#[tokio::test]
async fn the_unit_cap_ends_unfinished_work_without_success() {
    if !python_available() {
        return;
    }
    let run = run_eval(Plan::Rename, Some("2"), true, true).await;
    assert_eq!(run.sites_renamed(), 2, "{}", text(&run));
    let stops = run.turn_stops();
    assert_eq!(
        stops.iter().filter(|s| *s == "UnitCheckpoint").count(),
        1,
        "{stops:?}"
    );
    assert_ne!(run.exit_reason(), "Done");
}

#[tokio::test]
async fn an_opaque_launch_in_a_unit_earns_no_checkpoint() {
    if !python_available() {
        return;
    }
    let run = run_eval(Plan::ShellInUnit, Some("8"), true, true).await;
    assert!(
        !run.turn_stops().iter().any(|s| s == "UnitCheckpoint"),
        "{}",
        text(&run)
    );
    assert_ne!(run.exit_reason(), "Done");
}

#[tokio::test]
async fn a_denied_verifier_never_finishes_or_continues() {
    if !python_available() {
        return;
    }
    let run = run_eval(Plan::Rename, Some("8"), true, false).await;
    assert!(
        !run.turn_stops()
            .iter()
            .any(|s| s == "Done" || s == "UnitCheckpoint"),
        "{}",
        text(&run)
    );
}

#[tokio::test]
async fn a_project_file_cannot_enable_units() {
    let server = MockServer::start().await;
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    fixture(root.path(), config.path(), &server.uri());
    let project = root.path().join(".localpilot.toml");
    let mut body = std::fs::read_to_string(&project).unwrap();
    body.push_str("[harness.granularity]\nheadless_units = 8\n");
    std::fs::write(&project, body).unwrap();
    let workspace = root.path().to_path_buf();
    let user_config = config.path().to_path_buf();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_localpilot"))
            .current_dir(workspace)
            .env("APPDATA", &user_config)
            .env("XDG_CONFIG_HOME", &user_config)
            .args(["eval", "--model", "fixture", "--no-verify", "rename"])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("harness.granularity.headless_units"),
        "{stderr}"
    );
}

#[tokio::test]
async fn units_above_the_ceiling_are_a_configuration_error() {
    let run = run_eval(Plan::Rename, Some("9"), true, true).await;
    assert!(!run.output.status.success());
    assert!(
        String::from_utf8_lossy(&run.output.stderr).contains("headless_units"),
        "{}",
        text(&run)
    );
}
