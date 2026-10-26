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

const TEST: &str = "import contextlib\nimport io\nimport sys\nimport unittest\nsys.path.insert(0, '.')\n\n\nclass Rename(unittest.TestCase):\n    def test_every_site_uses_new_name(self):\n        import app\n        import money\n        self.assertFalse(hasattr(money, 'old_name'))\n        self.assertEqual(money.doubled(1.234), 2.46)\n        out = io.StringIO()\n        with contextlib.redirect_stdout(out):\n            app.main()\n        self.assertEqual(out.getvalue(), '3\\n')\n";

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
    std::fs::write(root.join("tests/test_rename.py"), TEST).unwrap();
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
    FailedShellInUnit,
    DeniedShellInUnit,
    ReviewAfterPass,
    NumericFirst,
    CallSubstitution,
    DirtyBaseline,
    FailedWrite,
    ProjectEnableRename,
    DifferentPair,
    WholeWriteFirst,
    WholeEditFirst,
    WholePatchFirst,
    WholeBatchFirst,
    WholeCallerBatchFirst,
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
        if self.plan == Plan::Rename && last["role"] == "user" {
            if let Some(note) = body["messages"].as_array().and_then(|messages| {
                messages
                    .iter()
                    .rev()
                    .map(|m| m["content"].to_string())
                    .find(|text| text.contains("Unit ") && text.contains("is recorded:"))
            }) {
                assert!(
                    note.contains("The write that ended this unit was refused"),
                    "{note}"
                );
                assert!(note.contains("was not executed"), "{note}");
                let completed: usize = note
                    .split("Unit ")
                    .nth(1)
                    .unwrap()
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                let applied = SITES
                    .iter()
                    .filter(|(file, old, _)| {
                        !std::fs::read_to_string(self.root.join(file))
                            .unwrap()
                            .contains(old)
                    })
                    .count();
                assert_eq!(
                    applied, completed,
                    "the refused next site must remain unapplied"
                );
            }
        }
        let whole_first = matches!(
            self.plan,
            Plan::WholeWriteFirst
                | Plan::WholeEditFirst
                | Plan::WholePatchFirst
                | Plan::WholeBatchFirst
                | Plan::WholeCallerBatchFirst
        );
        if whole_first && n == 0 {
            return calls_tool(n, "read_file", &json!({"path":"money.py"}));
        }
        if whole_first && n == 1 {
            let before = std::fs::read_to_string(self.root.join("money.py")).unwrap();
            let after = before.replace("old_name", "new_name");
            return match self.plan {
                Plan::WholeWriteFirst => {
                    calls_tool(n, "write_file", &json!({"path":"money.py","content":after}))
                }
                Plan::WholeEditFirst => calls_tool(
                    n,
                    "edit_file",
                    &json!({"path":"money.py","old_text":before,"new_text":after}),
                ),
                Plan::WholeBatchFirst | Plan::WholeCallerBatchFirst => {
                    let app = std::fs::read_to_string(self.root.join("app.py")).unwrap();
                    let mut edits = [
                        json!({"path":"money.py","old_text":before,"new_text":after}),
                        json!({"path":"app.py","old_text":app,"new_text":app.replace("old_name", "new_name")}),
                        json!({"path":"tests/test_rename.py","old_text":"hasattr(money, 'old_name')","new_text":"hasattr(money, 'new_name')"}),
                    ];
                    if self.plan == Plan::WholeCallerBatchFirst {
                        edits.swap(0, 1);
                    }
                    let calls: Vec<_> = edits.iter().enumerate().map(|(index, input)| json!({
                        "index":index,"id":format!("batch_{n}_{index}"),"function":{"name":"edit_file","arguments":input.to_string()}
                    })).collect();
                    sse(&[
                        json!({"choices":[{"delta":{"tool_calls":calls}}]}),
                        json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
                    ])
                }
                _ => calls_tool(
                    n,
                    "apply_patch",
                    &json!({"operations":[{"action":"update","path":"money.py","hunks":[{"old_text":before,"new_text":after}]}]}),
                ),
            };
        }
        if whole_first
            && (last_text.contains("Nothing was written")
                || last_text.contains("rename admission needs"))
        {
            let unchanged = std::fs::read_to_string(self.root.join("money.py")).unwrap();
            assert!(
                unchanged.contains("def old_name")
                    && unchanged.contains("return old_name(amount) * 2")
            );
            assert!(!unchanged.contains("new_name"));
            assert!(std::fs::read_to_string(self.root.join("app.py"))
                .unwrap()
                .contains("from money import old_name"));
            if !last_text.contains("rename admission needs") {
                assert!(last_text.contains("def new_name(amount):"), "{last_text}");
            }
            // Follow the suggested edit through ordinary dispatch below.
        } else if last["role"] == "tool" && last_text.contains("work envelope") {
            return says("This unit is done; the remaining old_name sites still need renaming.");
        }
        let remaining = SITES.iter().find(|(file, old, _)| {
            std::fs::read_to_string(self.root.join(file))
                .unwrap_or_default()
                .contains(old)
        });
        if self.plan == Plan::ReviewAfterPass && remaining.is_none() {
            if last_text.contains("this unit is at its limit") {
                // The full check passed: no unit opened, the edit stayed
                // refused, and the model is asked only for its answer.
                assert!(!last_text.contains("Unexecuted request"), "{last_text}");
                assert_eq!(
                    std::fs::read_to_string(self.root.join("tests/test_rename.py")).unwrap(),
                    TEST
                );
                return says("The source rename is complete; the original negative assertion correctly checks that the old name is absent.");
            }
            return calls_tool(
                n,
                "edit_file",
                &json!({"path":"tests/test_rename.py","old_text":"hasattr(money, 'old_name')","new_text":"hasattr(money, 'new_name')"}),
            );
        }
        let renamed_any = !std::fs::read_to_string(self.root.join("money.py"))
            .unwrap_or_default()
            .contains("def old_name");
        if self.plan == Plan::CallSubstitution && n == 0 {
            return calls_tool(
                n,
                "edit_file",
                &json!({"path":"money.py","old_text":"round(amount, 2)","new_text":"int(amount, 2)"}),
            );
        }
        if self.plan == Plan::NumericFirst && n == 0 {
            return calls_tool(
                n,
                "edit_file",
                &json!({"path":"money.py", "old_text":"round(amount, 2)", "new_text":"round(amount, 3)"}),
            );
        }
        if self.plan == Plan::FailedWrite && n == 0 {
            return calls_tool(
                n,
                "edit_file",
                &json!({"path":"money.py", "old_text":"not present", "new_text":"new_name"}),
            );
        }
        if self.plan == Plan::DifferentPair && renamed_any {
            if let Some((file, old, new)) = remaining {
                return calls_tool(
                    n,
                    "edit_file",
                    &json!({"path":file,"old_text":old,"new_text":new.replace("new_name","other_name")}),
                );
            }
        }
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
            (Plan::DeniedShellInUnit, _)
                if renamed_any && last["role"] == "tool" && last_text.contains("edited") =>
            {
                calls_tool(
                    n,
                    "run_shell",
                    &json!({"command":"python -c \"open('denied-shell-marker', 'w').write('ran')\""}),
                )
            }
            (Plan::FailedShellInUnit, _)
                if renamed_any && last["role"] == "tool" && last_text.contains("edited") =>
            {
                calls_tool(
                    n,
                    "run_shell",
                    &json!({"program":"git","args":["diff","--exit-code"]}),
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
    run_eval_mode(plan, units, None, verify, grant_verifier).await
}

async fn run_eval_mode(
    plan: Plan,
    units: Option<&str>,
    rename_units: Option<&str>,
    verify: bool,
    grant_verifier: bool,
) -> Run {
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
    if plan == Plan::ProjectEnableRename {
        let path = root.path().join(".localpilot.toml");
        let mut config = std::fs::read_to_string(&path).unwrap();
        config.push_str("\n[harness.granularity]\nheadless_rename_units = 8\n");
        std::fs::write(path, config).unwrap();
    }
    if plan == Plan::DirtyBaseline {
        std::fs::write(
            root.path().join("preexisting.txt"),
            "unrelated untracked material\n",
        )
        .unwrap();
    }
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
    let rename_units = rename_units.map(str::to_string);
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
            .env_remove("LOCALPILOT_HARNESS__GRANULARITY__HEADLESS_UNITS")
            .env_remove("LOCALPILOT_HARNESS__GRANULARITY__HEADLESS_RENAME_UNITS")
            .args(args);
        if let Some(units) = units {
            cmd.env("LOCALPILOT_HARNESS__GRANULARITY__HEADLESS_UNITS", units);
        }
        if let Some(units) = rename_units {
            cmd.env(
                "LOCALPILOT_HARNESS__GRANULARITY__HEADLESS_RENAME_UNITS",
                units,
            );
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
        assert!(all.contains(
            "If the original request still needs changes, continue with the next bounded unit"
        ));
        // The note keeps the scope rule instead of making a full pass the goal.
        assert!(all.contains("report unrelated existing failures instead of fixing them"));
        assert!(!all.contains(
            "complete only when you give your final answer and the full verification passes"
        ));
        assert!(all.contains("this call was not executed"), "{all}");
        assert!(
            all.contains("The write that ended this unit was refused"),
            "{all}"
        );
        assert!(all.contains("It is still pending"), "{all}");
    }
}

#[tokio::test]
async fn with_both_settings_off_the_task_stops_as_before() {
    if !python_available() {
        return;
    }
    for units in [None, Some("0")] {
        let run = run_eval_mode(Plan::Rename, units, Some("0"), true, true).await;
        assert_eq!(run.sites_renamed(), 1, "{}", text(&run));
        assert_eq!(run.exit_reason(), "NoProgress", "{}", text(&run));
        assert!(!run.turn_stops().iter().any(|s| s == "UnitCheckpoint"));
        let all = all_messages(&run);
        assert!(all.contains("verify it, and checkpoint before continuing"));
        assert!(!all.contains("this call was not executed"));
        assert!(!all.contains("next bounded unit"));
    }
}

#[tokio::test]
async fn an_answer_without_a_refused_write_never_checkpoints_or_finishes() {
    if !python_available() {
        return;
    }
    for verify in [true, false] {
        // One site, then answers only: the unit never reached its limit, so a
        // failed check stays ordinary feedback and no unit opens.
        let run = run_eval(Plan::StopAfterFirst, Some("8"), verify, true).await;
        let stops = run.turn_stops();
        assert!(
            stops.iter().all(|s| s != "UnitCheckpoint"),
            "{stops:?} {}",
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

#[tokio::test]
async fn selective_renames_finish_only_through_verified_units() {
    if !python_available() {
        return;
    }
    for verify in [true, false] {
        let run = run_eval_mode(Plan::Rename, None, Some("8"), verify, true).await;
        assert_eq!(run.sites_renamed(), 4, "{}", text(&run));
        assert_eq!(run.exit_reason(), "Done", "{}", text(&run));
        let messages = all_messages(&run);
        assert!(
            messages.contains("The write that ended this unit was refused"),
            "{messages}"
        );
        assert!(
            messages.contains("Only earlier successful edits remain applied"),
            "{messages}"
        );
        assert_eq!(
            run.turn_stops()
                .iter()
                .filter(|s| *s == "UnitCheckpoint")
                .count(),
            3,
            "{}",
            text(&run)
        );
    }
}

#[tokio::test]
async fn selective_admission_refuses_multi_hunk_writes_then_allows_bounded_retries() {
    if !python_available() {
        return;
    }
    for plan in [
        Plan::WholeWriteFirst,
        Plan::WholeEditFirst,
        Plan::WholePatchFirst,
        Plan::WholeBatchFirst,
        Plan::WholeCallerBatchFirst,
    ] {
        let run = run_eval_mode(plan, None, Some("8"), true, true).await;
        assert_eq!(run.sites_renamed(), 4, "{}", text(&run));
        assert_eq!(run.exit_reason(), "Done", "{}", text(&run));
        assert_eq!(
            run.turn_stops()
                .iter()
                .filter(|s| *s == "UnitCheckpoint")
                .count(),
            3,
            "{}",
            text(&run)
        );
        assert!(run
            .requests
            .iter()
            .any(|r| r.to_string().contains("Nothing was written")));
        let off = run_eval_mode(plan, None, Some("0"), true, true).await;
        assert_ne!(off.exit_reason(), "Done", "{}", text(&off));
        assert!(!off
            .requests
            .iter()
            .any(|r| r.to_string().contains("Nothing was written")));
    }
}

#[tokio::test]
async fn a_denied_opaque_call_does_not_invent_execution_or_block_verified_units() {
    if !python_available() {
        return;
    }
    for (global, selective) in [(Some("8"), None), (None, Some("8"))] {
        let run = run_eval_mode(Plan::DeniedShellInUnit, global, selective, true, true).await;
        assert!(all_messages(&run).contains("permission denied for run_shell"));
        assert!(!run.root.path().join("denied-shell-marker").exists());
        assert_eq!(run.sites_renamed(), 4, "{}", text(&run));
        assert_eq!(run.exit_reason(), "Done", "{}", text(&run));
    }
    let off = run_eval_mode(Plan::DeniedShellInUnit, None, Some("0"), true, true).await;
    assert_ne!(off.exit_reason(), "Done", "{}", text(&off));
    assert_eq!(off.sites_renamed(), 1, "{}", text(&off));
}

#[tokio::test]
async fn a_passing_full_check_keeps_a_queued_test_edit_refused() {
    if !python_available() {
        return;
    }
    for (global, selective) in [(Some("8"), None), (None, Some("8"))] {
        let run = run_eval_mode(Plan::ReviewAfterPass, global, selective, true, true).await;
        assert_eq!(run.sites_renamed(), 4, "{}", text(&run));
        assert_eq!(run.exit_reason(), "Done", "{}", text(&run));
        assert_eq!(
            std::fs::read_to_string(run.root.path().join("tests/test_rename.py")).unwrap(),
            TEST
        );
        // One checkpoint per failing site boundary; none after the pass.
        assert_eq!(
            run.turn_stops()
                .iter()
                .filter(|s| *s == "UnitCheckpoint")
                .count(),
            3
        );
        assert!(!all_messages(&run).contains("Reassess"));
    }
}

#[tokio::test]
async fn selective_mode_preserves_safety_and_source_identity_refusals() {
    if !python_available() {
        return;
    }
    for (plan, units, grant) in [
        (Plan::NumericFirst, "8", true),
        (Plan::CallSubstitution, "8", true),
        (Plan::DirtyBaseline, "8", true),
        (Plan::FailedWrite, "8", true),
        (Plan::ShellInUnit, "8", true),
        (Plan::FailedShellInUnit, "8", true),
        (Plan::StopAfterFirst, "8", true),
        (Plan::Rename, "8", false),
    ] {
        let run = run_eval_mode(plan, None, Some(units), true, grant).await;
        if plan == Plan::FailedShellInUnit {
            assert!(all_messages(&run).contains("exit: 1"), "{}", text(&run));
        }
        if plan == Plan::ShellInUnit {
            assert!(
                all_messages(&run).contains(r"tool: run_shell\nstatus: success"),
                "{}",
                text(&run)
            );
        }
        assert_ne!(run.exit_reason(), "Done", "{}", text(&run));
        assert!(
            !run.turn_stops().iter().any(|s| s == "UnitCheckpoint"),
            "{}",
            text(&run)
        );
    }
}

#[tokio::test]
async fn selective_unit_limit_and_zero_keep_work_unfinished() {
    if !python_available() {
        return;
    }
    for (units, sites, checkpoints) in [("0", 1, 0), ("2", 2, 1)] {
        let run = run_eval_mode(Plan::Rename, None, Some(units), true, true).await;
        assert_eq!(run.sites_renamed(), sites, "{}", text(&run));
        assert_ne!(run.exit_reason(), "Done", "{}", text(&run));
        assert_eq!(
            run.turn_stops()
                .iter()
                .filter(|s| *s == "UnitCheckpoint")
                .count(),
            checkpoints
        );
    }
}

#[tokio::test]
async fn explicit_global_units_keep_precedence_over_selective_mode() {
    if !python_available() {
        return;
    }
    // A numeric first change is ineligible for narrow mode, but legacy opt-in
    // global continuation still ends that bounded unit through its own checks.
    let run = run_eval_mode(Plan::NumericFirst, Some("2"), Some("8"), true, true).await;
    assert!(
        run.turn_stops().iter().any(|s| s == "UnitCheckpoint"),
        "{}",
        text(&run)
    );
}

#[tokio::test]
async fn selective_units_above_the_ceiling_fail_before_provider_requests() {
    let run = run_eval_mode(Plan::Rename, None, Some("9"), true, true).await;
    assert!(!run.output.status.success());
    assert!(run.requests.is_empty());
    assert!(text(&run).contains("headless_rename_units"));
}

#[tokio::test]
async fn by_default_eval_finishes_a_rename_through_selective_units() {
    if !python_available() {
        return;
    }
    // Neither setting given: selective rename units are on by default.
    let run = run_eval_mode(Plan::Rename, None, None, true, true).await;
    assert_eq!(run.sites_renamed(), 4, "{}", text(&run));
    assert_eq!(run.exit_reason(), "Done", "{}", text(&run));
    assert!(run.turn_stops().iter().any(|s| s == "UnitCheckpoint"));
}

#[tokio::test]
async fn a_project_cannot_enable_selective_continuation() {
    let run = run_eval_mode(Plan::ProjectEnableRename, None, None, true, true).await;
    assert!(!run.output.status.success());
    assert!(run.requests.is_empty());
    assert!(text(&run).contains("harness.granularity.headless_rename_units"));
}

#[tokio::test]
async fn a_selective_chain_cannot_switch_rename_pairs() {
    if !python_available() {
        return;
    }
    let run = run_eval_mode(Plan::DifferentPair, None, Some("8"), true, true).await;
    assert_ne!(run.exit_reason(), "Done", "{}", text(&run));
    assert_eq!(
        run.turn_stops()
            .iter()
            .filter(|s| *s == "UnitCheckpoint")
            .count(),
        1,
        "{}",
        text(&run)
    );
}
