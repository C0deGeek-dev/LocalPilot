"""Evaluate a model in LocalPilot's pair seat (`localpilot mesh run`).

Two subcommands:

    python drive.py check
    python drive.py run --model <served model name> --label <short name> \
        --cell owner --task roman --run 1 --out <results dir>

`check` needs no model. It verifies that the frozen fixtures are unchanged
(against FIXTURES.sha256) and still discriminate: every hidden test fails on
an empty tree, the roman hidden test passes on the clean review change and
fails on the planted one.

`run` drives one cell against a model server that is already serving
`--model` through LocalPilot's configured provider (LocalPilot's own config
decides the provider; `--provider` picks another). Cells:

- owner:        claude starts a pair session with the frozen spec of
                `--task`, hands the unit to localpilot, and
                `localpilot mesh run --own` implements it. At localpilot's
                first REVIEW_REQUEST the task's hidden test runs on the tree
                as observed; while the engine remains live, claude posts a
                scripted AGREE to let the unit close. Final journal facts are
                reconciled after exit without acting. If no live-request check
                ran, hidden acceptance is labeled as final-tree fallback.
                The hidden test is the measure, not claude's review.
- review-bad:   claude submits the roman change with a planted defect (bool
                accepted, against the spec); localpilot reviews. REVISE is
                expected, and an AGREE is a false AGREE.
- review-good:  the selected clean control. AGREE is expected; REVISE sets
                a raw expectation-mismatch flag, not finding adjudication.

`--review-case roman-v2` selects bool-covered clean tests and the unchanged
planted case. Legacy roman-v1 remains the default. See README.md for review
severity criteria and the legacy clean-control ambiguity.

Each run leaves its scratch repository and engine log under `--out`, and
appends one JSON line to `<out>/results.jsonl`.

It needs Python 3.9+, Git, and the `localpilot` binary (`--localpilot`, or
on PATH). The pair-protocol reference it drives claude with is the one
vendored beside this directory (`../conformance/reference/pair.py`).
"""

import argparse
import contextlib
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = pathlib.Path(__file__).resolve().parent
TASKS = HERE / "tasks"
REVIEW = HERE / "review"
PAIR = HERE.parent / "conformance" / "reference" / "pair.py"
TASK_NAMES = ("slug", "roman", "duration")
REVIEW_FILES = ("roman.py", "test_roman.py")
REVIEW_CASES = {
    "roman-v1": {"clean": REVIEW / "clean", "planted": REVIEW / "planted"},
    "roman-v2": {"clean": REVIEW / "roman-v2" / "clean", "planted": REVIEW / "planted"},
}
# Written into each scratch repository this driver creates; only a directory
# carrying it is ever deleted to make room for a rerun.
MARKER = ".seat-eval-run"
LABEL = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,39}")


def sha(path):
    """SHA-256 of a file with CRLF read as LF, so a checkout's line endings
    never change it."""
    return hashlib.sha256(path.read_bytes().replace(b"\r\n", b"\n")).hexdigest()


def run_cmd(cmd, cwd, check=True, env=None, timeout=None):
    p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, encoding="utf-8",
                       errors="replace", env=env, timeout=timeout)
    if check and p.returncode != 0:
        raise SystemExit(f"FAILED {cmd}: {p.stdout}{p.stderr}")
    return p


def hidden(repo, task):
    """Run a task's hidden test on `repo`: (passed, the output's tail)."""
    env = dict(os.environ, PYTHONPATH=str(repo))
    h = run_cmd([sys.executable, str(TASKS / task / "hidden.py")], repo, check=False, env=env,
                timeout=120)
    return h.returncode == 0 and "HIDDEN_OK" in h.stdout, (h.stdout + h.stderr)[-400:]


# --- check -------------------------------------------------------------------

def fixture_paths():
    return sorted(p for p in HERE.rglob("*") if p.is_file()
                  and p.parent != HERE and "__pycache__" not in p.parts)


def fixture_problems():
    """Everything wrong with the fixtures: a changed, missing or extra file,
    or a hidden test that no longer tells the good change from the bad."""
    problems = []
    manifest = HERE / "FIXTURES.sha256"
    want = {}
    for line in manifest.read_text(encoding="utf-8").splitlines():
        if line.strip():
            digest, rel = line.split(None, 1)
            want[rel] = digest
    have = {p.relative_to(HERE).as_posix(): sha(p) for p in fixture_paths()}
    for rel in sorted(set(want) | set(have)):
        if want.get(rel) != have.get(rel):
            problems.append(f"fixture {rel}: expected {want.get(rel)}, found {have.get(rel)}")
    with tempfile.TemporaryDirectory() as empty:
        for task in TASK_NAMES:
            ok, _ = hidden(pathlib.Path(empty), task)
            if ok:
                problems.append(f"the {task} hidden test passes on an empty tree")
    for case, sources in REVIEW_CASES.items():
        for name, expect in (("clean", True), ("planted", False)):
            with tempfile.TemporaryDirectory() as d:
                repo = pathlib.Path(d)
                for f in REVIEW_FILES:
                    shutil.copyfile(sources[name] / f, repo / f)
                visible = run_cmd([sys.executable, "-B", "-m", "unittest"], repo, check=False)
                if visible.returncode != 0:
                    problems.append(f"the {case} {name} visible tests fail: {visible.stderr[-400:]}")
                ok, tail = hidden(repo, "roman")
                if ok != expect:
                    problems.append(f"the roman hidden test {'fails' if expect else 'passes'} on the {case} {name} change: {tail}")
    return problems, len(have)


def check():
    problems, count = fixture_problems()
    for p in problems:
        print("PROBLEM", p)
    print("CHECK", "FAIL" if problems else "OK", f"({count} fixtures)")
    return 1 if problems else 0


# --- run ---------------------------------------------------------------------

def pair(repo, *args, check=True):
    return run_cmd([sys.executable, str(PAIR), "--repo", str(repo), *args], repo, check=check)


def journal(repo, role):
    out = []
    for j in (repo / ".pair-programming" / "sessions").glob(f"*/journal/{role}.jsonl"):
        for line in j.read_text(encoding="utf-8").splitlines():
            if line.strip():
                out.append(json.loads(line))
    return out


def remove_tree(path):
    """Delete a scratch repository. Git's object files are read-only on
    Windows, so clear that bit and retry; an entry that vanished meanwhile (a
    lock Git removed on its own) is already gone, which is the goal."""
    def retry(func, p, _exc):
        try:
            os.chmod(p, 0o700)
            func(p)
        except FileNotFoundError:
            pass
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=retry)
    else:
        shutil.rmtree(path, onerror=retry)


def scratch(path, spec):
    """A fresh scratch repository at `path`. An existing directory there is
    replaced only if this driver made it (it carries MARKER); anything else is
    refused and left alone."""
    if path.exists():
        if not (path / MARKER).is_file():
            raise SystemExit(f"refusing to replace {path}: not a scratch run this driver made")
        remove_tree(path)
    path.mkdir(parents=True)
    (path / MARKER).write_text("seat-eval scratch run\n", encoding="utf-8")
    run_cmd(["git", "init", "-q"], path)
    # No background maintenance: it would keep writing under .git while the
    # run, or a later rerun's delete, is using the repository.
    for k, v in [("user.email", "seat-eval@example.invalid"), ("user.name", "seat-eval"),
                 ("core.autocrlf", "false"), ("maintenance.auto", "false"), ("gc.auto", "0")]:
        run_cmd(["git", "config", k, v], path)
    (path / "README.md").write_text(spec, encoding="utf-8", newline="\n")
    (path / ".gitignore").write_text(f"__pycache__/\n{MARKER}\n", encoding="utf-8", newline="\n")
    run_cmd(["git", "add", "."], path)
    run_cmd(["git", "commit", "-qm", "base"], path)


def engine(a, repo, extra, log):
    env = dict(os.environ)
    env.pop("PAIR_REPO", None)
    if a.context_window:
        # The window LocalPilot budgets against; see the README.
        env[f"LOCALPILOT_PROVIDERS__{(a.provider or 'local').upper()}__CONTEXT_WINDOW"] = str(a.context_window)
    cmd = [a.localpilot, "mesh", "--repo", str(repo), "run", "--role", "localpilot",
           "--model", a.model, "--poll", "2", *extra]
    if a.provider:
        cmd += ["--provider", a.provider]
    if a.review_diagnostics:
        cmd += ["--review-diagnostics", str(repo.parent / f"{repo.name}.review.jsonl")]
    return subprocess.Popen(cmd, cwd=repo, stdout=log, stderr=subprocess.STDOUT, env=env)


@contextlib.contextmanager
def spawned(a, repo, extra, log):
    """The engine, stopped on the way out whatever happened, so no failure
    of the driver leaves it running."""
    proc = engine(a, repo, extra, log)
    try:
        yield proc
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def finish(proc, grace):
    try:
        proc.wait(timeout=grace)
        return False
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()
        return True


def owner_cell(a, repo, log):
    spec = (TASKS / a.task / "spec.md").read_text(encoding="utf-8")
    scratch(repo, spec)
    pair(repo, "start", "--role", "claude", "--with", "localpilot", "--task", spec)
    t0 = time.monotonic()
    r = {"hidden_ok": False, "review_requested": False, "escalated": False,
         "review_observation": None, "hidden_measure": None}
    r.update(wall_settings(a))
    with spawned(a, repo, ["--own", "--timeout", "120"], log) as proc:
        owner_loop(a, repo, proc, t0, r)
        loop_end = time.monotonic()
        loop_s = loop_end - t0
        r["owner_loop_s"] = round(loop_s, 3)
        r["owner_loop_deadline_reached"] = loop_s >= a.wall
        r["owner_loop_overrun_s"] = round(max(0, loop_s - a.wall), 3)
        r["killed"] = finish(proc, r["owner_exit_grace_s"])
        exit_at = time.monotonic()
        r["owner_exit_wait_s"] = round(exit_at - loop_end, 3)
        r["engine_exit"] = proc.returncode
    # The child can append its terminal message between polls. It is now
    # stopped, so this read cannot initiate another protocol action.
    owner_observation(repo, r, "after_exit")
    sessions = [p for name in ("session.json", "session.v2.json")
                for p in (repo / ".pair-programming" / "sessions").glob(f"*/{name}")]
    if len(sessions) > 1:
        raise RuntimeError("owner scratch run contains ambiguous session records")
    r["protocol_status"] = (json.loads(sessions[0].read_text(encoding="utf-8")).get("status")
                            if sessions else None)
    r["protocol_completed"] = (r["protocol_status"] == "completed"
                               if r["protocol_status"] is not None else None)
    r["wall_s"] = round(time.monotonic() - t0, 1)
    if r["hidden_measure"] is None:
        owner_hidden(a, repo, r, "final_tree_after_exit")
    r["post_exit_s"] = round(time.monotonic() - exit_at, 3)
    r["cell_wall_s"] = round(time.monotonic() - t0, 3)
    return r


def wall_settings(a):
    if getattr(a, "cell", "owner") != "owner":
        return {"wall_semantics": "review_engine_wait"}
    grace = getattr(a, "owner_exit_grace", None)
    return {"wall_semantics": "owner_loop_budget_plus_exit_grace",
            "owner_loop_budget_s": a.wall,
            "owner_exit_grace_s": 60 if grace is None else grace}


def owner_hidden(a, repo, r, measure):
    started = time.monotonic()
    r["hidden_ok"], r["hidden_tail"] = hidden(repo, a.task)
    r["hidden_check_s"] = round(time.monotonic() - started, 3)
    r["hidden_measure"] = measure


def owner_observation(repo, r, phase):
    """Record journal facts independently; observing is never acknowledging."""
    lp = journal(repo, "localpilot")
    r["escalated"] = r["escalated"] or any(m["kind"] == "ESCALATE" for m in lp)
    req = next((m for m in lp if m["kind"] == "REVIEW_REQUEST"), None)
    if req is not None and not r["review_requested"]:
        r["review_requested"] = True
        r["review_observation"] = phase
        r["review_request_id"] = req.get("msg_id") or f"localpilot:{req['seq']}"
    return req


def owner_loop(a, repo, proc, t0, r):
    offered = False
    while time.monotonic() - t0 < a.wall and proc.poll() is None:
        if not offered and "localpilot: ready" in pair(repo, "status", check=False).stdout:
            offered = pair(repo, "handoff-offer", "--role", "claude", check=False).returncode == 0
        req = owner_observation(repo, r, "live" if proc.poll() is None else "after_exit")
        if r["escalated"] or proc.poll() is not None:
            break
        if req is not None and r["hidden_measure"] is None:
            r["request_at_s"] = round(time.monotonic() - t0, 1)
            owner_hidden(a, repo, r, "first_request_observed_tree")
            if proc.poll() is not None:
                break
            pair(repo, "watch", "--role", "claude", "--timeout", "5", check=False)
            if proc.poll() is not None:
                break
            pair(repo, "post", "--role", "claude", "--kind", "VERDICT", "--reply-to", r["review_request_id"],
                 "--body", "AGREE round=1 blocking=0 important=0\n"
                           "Scripted AGREE: the hidden test is the measure of this run.",
                 check=False)
        time.sleep(min(3, max(0, a.wall - (time.monotonic() - t0))))


def review_cell(a, repo, log, planted):
    spec = (TASKS / "roman" / "spec.md").read_text(encoding="utf-8")
    scratch(repo, spec)
    pair(repo, "start", "--role", "claude", "--with", "localpilot", "--task", spec)
    src = REVIEW_CASES[a.review_case]["planted" if planted else "clean"]
    for f in REVIEW_FILES:
        shutil.copyfile(src / f, repo / f)
    body = ("roman.py and test_roman.py implement the task spec.\n"
            "Tests: python -m unittest, rc=0.\n\nFingerprints:\n"
            + "\n".join(f"{f}={sha(repo / f)[:12]}" for f in REVIEW_FILES))
    # The request is waiting when the engine joins; `--once` ends the engine
    # after it has answered that one delivery.
    pair(repo, "post", "--role", "claude", "--kind", "REVIEW_REQUEST", "--expect-reply", "--body", body)
    t0 = time.monotonic()
    with spawned(a, repo, ["--once", "--timeout", "120"], log) as proc:
        killed = finish(proc, a.wall)
    lp = journal(repo, "localpilot")
    verdicts = [m for m in lp if m["kind"] == "VERDICT"]
    decision = verdicts[-1]["body"].split()[0] if verdicts else None
    return {
        "decision": decision,
        "expected": "REVISE" if planted else "AGREE",
        "false_agree": planted and decision == "AGREE",
        "false_revise": (not planted) and decision == "REVISE",
        "no_verdict": decision is None,
        "escalated": any(m["kind"] == "ESCALATE" for m in lp),
        "verdict": verdicts[-1]["body"] if verdicts else None,
        "killed": killed,
        "engine_exit": proc.returncode,
        "wall_s": round(time.monotonic() - t0, 1),
    }


def run_path(out, a):
    """The run's name and scratch path, which must be a direct child of
    `out`."""
    grace = getattr(a, "owner_exit_grace", None)
    if a.wall < 0 or (grace is not None and grace < 0):
        raise SystemExit("--wall and --owner-exit-grace must be nonnegative")
    if a.cell != "owner" and grace is not None:
        raise SystemExit("--owner-exit-grace applies only to owner cells")
    if not LABEL.fullmatch(a.label):
        raise SystemExit(f"--label must be 1-40 of A-Z a-z 0-9 . _ - and start with a letter or digit: {a.label!r}")
    if a.run < 1:
        raise SystemExit("--run must be 1 or more")
    if a.review_case not in REVIEW_CASES:
        raise SystemExit(f"unknown review case: {a.review_case!r}")
    if a.cell == "owner" and a.review_case != "roman-v1":
        raise SystemExit("--review-case applies only to review cells")
    if a.cell == "owner" and a.review_diagnostics:
        raise SystemExit("--review-diagnostics applies only to review cells")
    suffix = f"-{a.task}" if a.cell == "owner" else (
        f"-{a.review_case}" if a.review_case != "roman-v1" else "")
    name = f"{a.label}-{a.cell}{suffix}-{a.run}"
    repo = (out / name).resolve()
    if repo.parent != out.resolve():
        raise SystemExit(f"refusing a run path outside {out}: {repo}")
    return name, repo


def occupied(out, name):
    """Whether any trace of run `name` exists under `out`."""
    if (out / name).exists() or (out / f"{name}.log").exists() or (out / f"{name}.review.jsonl").exists():
        return True
    results = out / "results.jsonl"
    if results.is_file():
        for line in results.read_text(encoding="utf-8").splitlines():
            try:
                if json.loads(line).get("name") == name:
                    return True
            except ValueError:
                continue
    return False


def turn_metadata(log_path, result):
    """Observed public trace metadata, independent of response capture/scoring.

    A killed/failed or legacy trace is explicitly incomplete. Never substitute
    a built-in default for an effective value missing from the runtime log.
    """
    text = log_path.read_text(encoding="utf-8", errors="replace")
    deadlines = [{"seconds": None if seconds == "none" else int(seconds),
                  "source": source}
                 for seconds, source in re.findall(
                     r"^  TURN_RAILS turn_timeout_secs=(none|[0-9]+) "
                     r"turn_timeout_source=(builtin|config)$", text, re.M)]
    stops = re.findall(r"^  TURN ended ([A-Za-z]+)$", text, re.M)
    return {
        "runtime_turn_deadlines": deadlines,
        "runtime_turn_stops": stops,
        "runtime_turn_timeouts": stops.count("TimedOut") if deadlines or stops else None,
        "runtime_trace_complete": bool(deadlines)
            and len(deadlines) == len(stops)
            and not result.get("killed", False)
            and result.get("engine_exit") == 0
            and "driver_error" not in result,
    }


def run(a):
    out = pathlib.Path(a.out).resolve()
    name, repo = run_path(out, a)
    # Results are comparable only on the frozen fixtures.
    problems, _ = fixture_problems()
    if problems:
        raise SystemExit("refusing to run on changed fixtures:\n" + "\n".join(problems))
    # A run name is used once: its repository, log and results row are the
    # evidence for that row, so a repeat takes a new --run.
    if occupied(out, name):
        raise SystemExit(f"run {name} already exists under {out}; use another --run")
    review_identity = {}
    if a.cell != "owner":
        src = REVIEW_CASES[a.review_case]["planted" if a.cell == "review-bad" else "clean"]
        review_identity = {
            "review_case": a.review_case,
            "review_fixture_hashes": {f: sha(src / f) for f in REVIEW_FILES},
            "review_spec_hash": sha(TASKS / "roman" / "spec.md"),
            "review_diagnostics": f"{name}.review.jsonl" if a.review_diagnostics else None,
        }
    out.mkdir(parents=True, exist_ok=True)
    started = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    with open(out / f"{name}.log", "x", encoding="utf-8") as log:
        try:
            if a.cell == "owner":
                r = owner_cell(a, repo, log)
            else:
                r = review_cell(a, repo, log, a.cell == "review-bad")
        except BaseException as e:
            # Record the failure against the run, then fail loudly; the
            # engine has already been stopped.
            r = {"driver_error": f"{type(e).__name__}: {e}"[:400]}
            r.update({"name": name, "label": a.label, "model": a.model, "cell": a.cell,
                      "run": a.run, "started": started, "wall_cap_s": a.wall})
            log.flush()
            r.update(turn_metadata(out / f"{name}.log", r))
            r.update(review_identity)
            r.update(wall_settings(a))
            if a.cell != "owner":
                r["review_diagnostics_present"] = (out / f"{name}.review.jsonl").is_file() if a.review_diagnostics else False
            with open(out / "results.jsonl", "a", encoding="utf-8") as f:
                f.write(json.dumps(r) + "\n")
            raise
    r.update({"name": name, "label": a.label, "model": a.model, "cell": a.cell,
              "task": a.task if a.cell == "owner" else "roman", "run": a.run,
              "started": started, "wall_cap_s": a.wall})
    r.update(review_identity)
    r.update(wall_settings(a))
    r.update(turn_metadata(out / f"{name}.log", r))
    if a.cell != "owner":
        r["review_diagnostics_present"] = (out / f"{name}.review.jsonl").is_file() if a.review_diagnostics else False
    with open(out / "results.jsonl", "a", encoding="utf-8") as f:
        f.write(json.dumps(r) + "\n")
    print(json.dumps({k: r[k] for k in r if k not in ("verdict", "hidden_tail")}))
    return 0


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("check", help="verify the fixtures; needs no model")
    r = sub.add_parser("run", help="drive one cell against a served model")
    r.add_argument("--model", required=True, help="the model name the server serves")
    r.add_argument("--label", required=True, help="a short name for the results")
    r.add_argument("--cell", required=True, choices=["owner", "review-bad", "review-good"])
    r.add_argument("--review-case", default="roman-v1", choices=tuple(REVIEW_CASES),
                   help="review fixture version (default: legacy roman-v1; owner cells unchanged)")
    r.add_argument("--review-diagnostics", action="store_true",
                   help="opt into bounded redacted attempt capture beside the engine log (review cells only)")
    r.add_argument("--task", default="roman", choices=TASK_NAMES, help="owner cell: the frozen task")
    r.add_argument("--run", type=int, required=True, help="the run's number within its cell")
    r.add_argument("--out", required=True, help="the results directory")
    r.add_argument("--wall", type=int, default=2700,
                   help="seconds of owner-loop budget (checked between synchronous operations), "
                        "or review-engine wait budget; excludes setup and final assessment (default: 2700)")
    r.add_argument("--owner-exit-grace", type=int,
                   help="owner only: additional seconds to wait after the loop before killing "
                        "the engine (default: 60; zero skips grace)")
    r.add_argument("--localpilot", default=shutil.which("localpilot") or "localpilot")
    r.add_argument("--provider", help="a configured LocalPilot provider (default: the default one)")
    r.add_argument("--context-window", type=int, help="override the provider's context window")
    a = ap.parse_args()
    if a.cmd == "check":
        return check()
    # The engine starts inside each scratch repository, so the binary must
    # not be a path relative to here.
    found = shutil.which(a.localpilot)
    if found is None:
        raise SystemExit(f"no localpilot binary at {a.localpilot!r}")
    a.localpilot = str(pathlib.Path(found).resolve())
    return run(a)


if __name__ == "__main__":
    sys.exit(main())
