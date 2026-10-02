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
                as it is; claude then posts a scripted AGREE so the unit
                closes. The hidden test is the measure, not claude's review.
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
    r = {"hidden_ok": False, "review_requested": False, "escalated": False}
    with spawned(a, repo, ["--own", "--timeout", "120"], log) as proc:
        owner_loop(a, repo, proc, t0, r)
        r["killed"] = finish(proc, 60)
        r["engine_exit"] = proc.returncode
    r["wall_s"] = round(time.monotonic() - t0, 1)
    if not r["review_requested"]:
        r["hidden_ok"], r["hidden_tail"] = hidden(repo, a.task)
    return r


def owner_loop(a, repo, proc, t0, r):
    offered = False
    while time.monotonic() - t0 < a.wall and proc.poll() is None:
        if not offered and "localpilot: ready" in pair(repo, "status", check=False).stdout:
            offered = pair(repo, "handoff-offer", "--role", "claude", check=False).returncode == 0
        lp = journal(repo, "localpilot")
        if any(m["kind"] == "ESCALATE" for m in lp):
            r["escalated"] = True
            break
        req = [m for m in lp if m["kind"] == "REVIEW_REQUEST"]
        if req and not r["review_requested"]:
            r["review_requested"] = True
            r["hidden_ok"], r["hidden_tail"] = hidden(repo, a.task)
            r["request_at_s"] = round(time.monotonic() - t0, 1)
            mid = req[0].get("msg_id") or f"localpilot:{req[0]['seq']}"
            pair(repo, "watch", "--role", "claude", "--timeout", "5", check=False)
            pair(repo, "post", "--role", "claude", "--kind", "VERDICT", "--reply-to", mid,
                 "--body", "AGREE round=1 blocking=0 important=0\n"
                           "Scripted AGREE: the hidden test is the measure of this run.",
                 check=False)
        time.sleep(3)


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
    if not LABEL.fullmatch(a.label):
        raise SystemExit(f"--label must be 1-40 of A-Z a-z 0-9 . _ - and start with a letter or digit: {a.label!r}")
    if a.run < 1:
        raise SystemExit("--run must be 1 or more")
    if a.review_case not in REVIEW_CASES:
        raise SystemExit(f"unknown review case: {a.review_case!r}")
    if a.cell == "owner" and a.review_case != "roman-v1":
        raise SystemExit("--review-case applies only to review cells")
    suffix = f"-{a.task}" if a.cell == "owner" else (
        f"-{a.review_case}" if a.review_case != "roman-v1" else "")
    name = f"{a.label}-{a.cell}{suffix}-{a.run}"
    repo = (out / name).resolve()
    if repo.parent != out:
        raise SystemExit(f"refusing a run path outside {out}: {repo}")
    return name, repo


def occupied(out, name):
    """Whether any trace of run `name` exists under `out`."""
    if (out / name).exists() or (out / f"{name}.log").exists():
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
                      "run": a.run, "started": started})
            r.update(review_identity)
            with open(out / "results.jsonl", "a", encoding="utf-8") as f:
                f.write(json.dumps(r) + "\n")
            raise
    r.update({"name": name, "label": a.label, "model": a.model, "cell": a.cell,
              "task": a.task if a.cell == "owner" else "roman", "run": a.run,
              "started": started, "wall_cap_s": a.wall})
    r.update(review_identity)
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
    r.add_argument("--task", default="roman", choices=TASK_NAMES, help="owner cell: the frozen task")
    r.add_argument("--run", type=int, required=True, help="the run's number within its cell")
    r.add_argument("--out", required=True, help="the results directory")
    r.add_argument("--wall", type=int, default=2700, help="seconds before a run is cut off")
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
