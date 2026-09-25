#!/usr/bin/env python3
"""Run the pair-programming conformance fixtures against a command-line
implementation (by default this skill's own `scripts/pair.py`).

Each fixture runs in a fresh, disposable repository under the system temp
directory, never in the checkout that invoked it. See README.md for the
fixture format, the two layers (STATE and CLI) and how to add a fixture.

    python run.py                          # every fixture, pair.py
    python run.py --profile two-party      # only fixtures tagged two-party
    python run.py --impl localpilot=<cmd>  # mix: that role's steps use <cmd>
    python run.py --capture <fixture>      # fill expected values from the reference
"""
from __future__ import annotations
import argparse, json, os, re, shlex, stat, subprocess, sys, tempfile, shutil
from pathlib import Path

HERE = Path(__file__).resolve().parent
FIXTURES = HERE / "fixtures"
# The reference implementation: the skill's own `scripts/pair.py`, or, in a
# vendored copy of this suite, the pinned test-only copy beside it
# (`reference/pair.py`, listed in MANIFEST.json). `--reference` overrides both.
_VENDORED_REFERENCE = HERE / "reference" / "pair.py"
REFERENCE = _VENDORED_REFERENCE if _VENDORED_REFERENCE.is_file() else HERE.parent / "scripts" / "pair.py"
MAILBOX = ".pair-programming"
STEP_TIMEOUT = 120
# A fixed date makes the base commit, and so `base_head`, the same on every run.
GIT_DATE = "2026-01-01T00:00:00Z"


class Refused(Exception):
    """A fixture asked for something the runner will not do."""


# --- normalisation ------------------------------------------------------------

SID = re.compile(r"\d{8}T\d{6}Z-[0-9a-f]{8}")
TS = re.compile(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z")
UNIT_SUFFIX = re.compile(r"\b(\d+)-[0-9a-f]{8}\b")
SHOWN_UNIT = re.compile(r"#(\d+)-[0-9a-f]{8}")
SHA = re.compile(r"\b[0-9a-f]{40}\b")
HEX64 = re.compile(r"\b[0-9a-f]{64}\b")
# The impls key for steps that name no role (status, transcript): in
# participant mode they run on the implementation under test.
OBSERVER = "<observer>"
# The operations a participant-profile implementation provides (spec
# "Conformance"). Everything else (start, park, resume, purge, verify-request)
# stays with the reference.
PARTICIPANT_OPS = frozenset({"join", "post", "watch", "peek", "ack", "status", "transcript", "health",
                             "handoff-offer", "handoff-accept", "next-unit", "complete", "guard-write",
                             "endpoint", "accept", "record-push"})
MANDATORY_FILE = HERE / "participant.json"
# Environment a step may set, from a value an earlier step printed. Nothing
# else: in particular never PAIR_REPO.
STEP_ENV = {"PAIR_ENDPOINT_TOKEN"}


def normalise_text(text: str, root: Path) -> str:
    """Replace what differs between runs of the same build: the fixture root,
    session ids, times, unit id suffixes and commit ids. Nothing else."""
    for form in {str(root), str(root).replace("\\", "/"), root.as_posix()}:
        text = text.replace(form, "<REPO>")
    text = SID.sub("<SID>", text)
    text = TS.sub("<TS>", text)
    text = SHOWN_UNIT.sub(r"#\1", text)
    text = UNIT_SUFFIX.sub(r"\1-<U>", text)
    text = HEX64.sub("<HEX64>", text)
    text = SHA.sub("<SHA>", text)
    return text.replace("\r\n", "\n")


def normalise_value(v, root: Path):
    if isinstance(v, str):
        return normalise_text(v, root)
    if isinstance(v, list):
        return [normalise_value(x, root) for x in v]
    if isinstance(v, dict):
        return {normalise_text(k, root): normalise_value(x, root) for k, x in v.items()}
    return v


def journal_entries(text: str, root: Path) -> list:
    """A journal as the STATE layer sees it: one entry per line, split on "\\n"
    only. A line that is not a JSON object is kept as {"$invalid": text}; a last
    line with no terminator is followed by the marker {"$no_eol": true}."""
    parts = text.split("\n")
    tail_open = parts[-1] != ""
    if not tail_open:
        parts = parts[:-1]
    out = []
    for line in parts:
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            rec = None
        out.append(normalise_value(rec, root) if isinstance(rec, dict) else {"$invalid": normalise_text(line, root)})
    if tail_open:
        out.append({"$no_eol": True})
    return out


def session_labels(base: Path) -> dict:
    """A distinct, run-independent label for each session in the mailbox.

    Every session id normalises to `<SID>`, which is right for one session but
    would merge two sessions' files under one key and hide one of them. The
    session the pointer names stays `<SID>`; any other is `<SID-2>`, `<SID-3>`,
    ordered by its record's `created_at`, then its work unit."""
    sessions = base / "sessions"
    if not sessions.is_dir():
        return {}
    active = None
    for name in ("active.v2.json", "active.json"):
        try:
            active = json.loads((base / name).read_text(encoding="utf-8")).get("session_id")
            break
        except (OSError, ValueError, AttributeError):
            continue
    if active is None:
        try:
            text = (base / "active.json").read_text(encoding="utf-8")
            active = SID.search(text).group(0) if SID.search(text) else None
        except OSError:
            pass
    def key(d):
        rec = {}
        for f in ("session.v2.json", "session.json"):
            try:
                rec = json.loads((d / f).read_text(encoding="utf-8")); break
            except (OSError, ValueError):
                continue
        rec = rec if isinstance(rec, dict) else {}
        return (str(rec.get("created_at", "")), str(rec.get("work_unit", "")), d.name)
    others = sorted((d for d in sessions.iterdir() if d.is_dir() and d.name != active), key=key)
    labels = {active: "<SID>"} if active and (sessions / active).is_dir() else {}
    for d in others:
        labels[d.name] = "<SID>" if not labels else f"<SID-{len(labels) + 1}>"
    return labels


def snapshot(root: Path) -> dict:
    """The normalised mailbox tree: relative path -> parsed content."""
    base = root / MAILBOX
    out = {}
    if not base.exists():
        return out
    labels = session_labels(base)
    def relabel(text: str) -> str:
        for sid, label in labels.items():
            text = text.replace(sid, label)
        return text
    for p in sorted(base.rglob("*")):
        if not p.is_file():
            continue
        rel = normalise_text(relabel(p.relative_to(base).as_posix()), root)
        raw = relabel(p.read_bytes().decode("utf-8", errors="replace"))
        if p.suffix == ".jsonl":
            out[rel] = journal_entries(raw, root)
        elif p.suffix == ".json":
            try:
                out[rel] = normalise_value(json.loads(raw), root)
            except json.JSONDecodeError:
                out[rel] = {"$text": normalise_text(raw, root)}
        elif p.name.endswith(".lock"):
            out[rel] = "<LOCK>"
        else:
            out[rel] = {"$text": normalise_text(raw, root)}
    return out


# --- containment --------------------------------------------------------------

def _inside(child: Path, parent: Path) -> bool:
    try:
        child.relative_to(parent)
        return True
    except ValueError:
        return False


def _is_link(p: Path) -> bool:
    try:
        st = os.lstat(p)
    except FileNotFoundError:
        return False
    if stat.S_ISLNK(st.st_mode):
        return True
    attrs = getattr(st, "st_file_attributes", 0)
    return bool(attrs & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400))


def confine(root: Path, rel: str) -> Path:
    """The absolute path of a `raw` step's target, or Refused. Checked before
    any I/O: only a relative path inside the fixture's mailbox, reached without
    crossing a symlink, junction or other reparse point."""
    if not rel or "\0" in rel:
        raise Refused("RAW_PATH_REFUSED empty or NUL path")
    if rel.startswith(("/", "\\")) or re.match(r"^[A-Za-z]:", rel) or rel.startswith("\\\\"):
        raise Refused(f"RAW_PATH_REFUSED absolute path {rel!r}")
    parts = re.split(r"[\\/]+", rel)
    if ".." in parts:
        raise Refused(f"RAW_PATH_REFUSED parent segment in {rel!r}")
    base = root / MAILBOX
    cur = base
    for part in [p for p in parts if p not in ("", ".")]:
        if _is_link(cur):
            raise Refused(f"RAW_PATH_REFUSED link on the way to {rel!r}")
        cur = cur / part
    if _is_link(cur):
        raise Refused(f"RAW_PATH_REFUSED link at {rel!r}")
    real_base = Path(os.path.realpath(base))
    if not _inside(Path(os.path.realpath(cur.parent)), real_base):
        raise Refused(f"RAW_PATH_REFUSED {rel!r} resolves outside the mailbox")
    return cur


def raw_step(root: Path, op: dict) -> None:
    target = confine(root, op.get("path", ""))
    kind = op.get("op")
    if kind in ("remove", "truncate") and target.exists() and not target.is_file():
        raise Refused(f"RAW_PATH_REFUSED {kind} of a non-file {op['path']!r}")
    # `data_hex` writes exact bytes, for damage text cannot express (a
    # multi-byte character cut in half by a crash).
    data = bytes.fromhex(op["data_hex"]) if "data_hex" in op else op.get("data", "").encode("utf-8")
    if kind == "append_bytes":
        target.parent.mkdir(parents=True, exist_ok=True)
        with open(target, "ab") as f:
            f.write(data)
    elif kind == "write_file":
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
    elif kind == "truncate":
        if target.exists():
            with open(target, "r+b") as f:
                f.truncate(int(op.get("size", 0)))
    elif kind == "remove":
        if target.exists():
            target.unlink()
    elif kind == "json_set":
        # Set one top-level key of an existing JSON object file, as a newer
        # build (or damage) would leave it.
        if not target.is_file():
            raise Refused(f"RAW_PATH_REFUSED json_set needs an existing file {op['path']!r}")
        obj = json.loads(target.read_text(encoding="utf-8"))
        obj[op["key"]] = op["value"]
        target.write_text(json.dumps(obj, ensure_ascii=False, separators=(",", ":")) + "\n", encoding="utf-8")
    elif kind == "make_lock":
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(op.get("data", "0 1970-01-01T00:00:00Z\n"), encoding="utf-8")
        if op.get("age_s"):
            # A lock left by a dead holder: old enough to be reaped (spec L-5).
            import time
            t = time.time() - float(op["age_s"])
            os.utime(target, (t, t))
    else:
        raise Refused(f"RAW_OP_REFUSED unknown op {kind!r}")


def resolve_session_path(root: Path, rel: str) -> str:
    """`raw` paths may say <SID> for the active session's id."""
    if "<SID>" not in rel:
        return rel
    base = root / MAILBOX
    ptr = base / "active.v2.json"
    if not ptr.exists():
        ptr = base / "active.json"
    sid = json.loads(ptr.read_text(encoding="utf-8"))["session_id"]
    return rel.replace("<SID>", sid)


def new_root(vcs: str) -> Path:
    root = Path(tempfile.mkdtemp(prefix="pair-conformance-")).resolve()
    here = Path.cwd().resolve()
    if _inside(root, here):
        shutil.rmtree(root, ignore_errors=True)
        raise Refused(f"ROOT_REFUSED {root} is inside the invoking directory")
    probe = subprocess.run(["git", "-C", str(root), "rev-parse", "--is-inside-work-tree"],
                           capture_output=True, text=True)
    if probe.returncode == 0:
        shutil.rmtree(root, ignore_errors=True)
        raise Refused(f"ROOT_REFUSED {root} is inside a git work tree")
    if vcs == "git":
        env = dict(os.environ, GIT_AUTHOR_DATE=GIT_DATE, GIT_COMMITTER_DATE=GIT_DATE)
        subprocess.run(["git", "init", "-q", str(root)], check=True)
        for k, v in (("user.email", "pair@example.invalid"), ("user.name", "pair-test")):
            subprocess.run(["git", "-C", str(root), "config", k, v], check=True)
        (root / "README.md").write_text("base\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(root), "add", "README.md"], check=True)
        subprocess.run(["git", "-C", str(root), "commit", "-qm", "base"], check=True, env=env)
    return root


# --- running ------------------------------------------------------------------

def child_env(extra: dict | None = None) -> dict:
    e = dict(os.environ)
    e.pop("PAIR_REPO", None)  # never let a step reach the live repository
    for k in STEP_ENV:
        e.pop(k, None)
    for k, v in (extra or {}).items():
        if k not in STEP_ENV:
            raise Refused(f"ENV_REFUSED a step may not set {k!r}")
        e[k] = v
    return e


def step_env(st: dict, vars_: dict) -> dict:
    """A step's `env`, with `${NAME}` replaced by what an earlier step captured."""
    out = {}
    for k, v in (st.get("env") or {}).items():
        out[k] = re.sub(r"\$\{(\w+)\}", lambda m: vars_.get(m.group(1), ""), v)
    return out


def argv_for(impls: dict, root: Path, cmd: list) -> list:
    """The implementation chosen for the step's --role (or '*'), then
    --repo <fixture root>, then the step's own arguments."""
    check_argv(cmd)
    role = cmd[cmd.index("--role") + 1] if "--role" in cmd else OBSERVER
    return [*(impls.get(role) or impls["*"]), "--repo", str(root), *cmd]


def invoke(impls: dict, root: Path, cmd: list, env: dict | None = None):
    # `<SID>` in an argument stands for the active session's id, as in raw paths.
    if any("<SID>" in x for x in cmd):
        cmd = [resolve_session_path(root, x) for x in cmd]
    return subprocess.run(argv_for(impls, root, cmd), text=True, encoding="utf-8", errors="replace",
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          env=child_env(env), timeout=STEP_TIMEOUT)


def entries(t: str) -> list:
    """A transcript as a set of entries: it orders by a one-second timestamp,
    then role, so same-second entries may reorder between runs."""
    return sorted(("\n\n" + t.rstrip("\n")).split("\n\n## ")[1:])


PEER_HEAD = re.compile(r"^(?=PEER [a-z]+ #\d+ )", re.M)
PEER_ID = re.compile(r"^PEER ([a-z]+) #(\d+) ")


def blocks(t: str) -> tuple:
    """Peer mail as (sorted blocks, per-sender seq order). Mail from different
    senders is ordered by a one-second timestamp, then role, so its interleaving
    may differ between runs; each sender's own order may not."""
    parts = [x for x in PEER_HEAD.split(t) if x]
    order = {}
    for x in parts:
        m = PEER_ID.match(x)
        if m:
            order.setdefault(m.group(1), []).append(int(m.group(2)))
    return sorted(x.rstrip("\n") for x in parts), order


OBS_LINES = ("ENDPOINT ", "ENDPOINT_TOKEN=", "ACCEPTED ", "PUSH_RECORDED ")
STATUS_LINES = ("SESSION ", "ENDPOINT ", "JOURNAL_INVALID ", "WAITING ", "AUTHORITY ", "PAUSE ")
HEALTH_LINE = re.compile(r"^[a-z]+: \S+ updated=")
ERR_LINES = ("REFUSED ", "WRITE_DENIED ")


def obs_check(st: dict, rc: int, out: str, err: str) -> list:
    """The observable checks for an implementation that is not a CLI drop-in:
    what a model reads or a caller acts on must match, even where the full
    output need not. Exit codes always; peer mail as blocks; plumbing lines;
    status's required lines as a subset when it succeeds; refusal lines."""
    op = st["cmd"][0]
    bad = []
    if st["rc"] != rc:
        bad.append(f"rc {rc} != {st['rc']}")
    if op in ("watch", "peek") and blocks(st["stdout"]) != blocks(out):
        bad.append(f"peer mail differs\n--- expected\n{st['stdout']}--- got\n{out}")
    pick = lambda t, pre: sorted(l for l in t.splitlines() if l.startswith(pre))
    if op in ("endpoint", "accept", "record-push") and pick(st["stdout"], OBS_LINES) != pick(out, OBS_LINES):
        bad.append(f"plumbing lines differ\n--- expected\n{st['stdout']}--- got\n{out}")
    # A status that refused may have printed some lines first; what it
    # printed before refusing is not part of the contract.
    if op == "status" and st["rc"] == 0:
        need = {l for l in st["stdout"].splitlines() if l.startswith(STATUS_LINES) or HEALTH_LINE.match(l)}
        missing = sorted(need - set(out.splitlines()))
        if missing:
            bad.append("status lacks required lines: " + " | ".join(missing))
    if pick(st["stderr"], ERR_LINES) != pick(err, ERR_LINES):
        bad.append(f"refusal lines differ\n--- expected\n{st['stderr']}--- got\n{err}")
    return bad


def fixture_roles(fx: dict) -> set:
    roles = set()
    for st in fx["steps"]:
        for cmd in ([st["cmd"]] if "cmd" in st else st.get("parallel", [])):
            if "--role" in cmd:
                roles.add(cmd[cmd.index("--role") + 1])
    return roles


def participant_selection(fx: dict, native: list) -> tuple:
    """(roles run natively, None) when the fixture fits the participant
    profile, or (None, reason) when it does not."""
    roles = [r for r in native if r in fixture_roles(fx)]
    if not roles:
        return None, f"none of {','.join(native)} takes part"
    for st in fx["steps"]:
        for cmd in ([st["cmd"]] if "cmd" in st else st.get("parallel", [])):
            role = cmd[cmd.index("--role") + 1] if "--role" in cmd else OBSERVER
            if (role in roles or role == OBSERVER) and cmd[0] not in PARTICIPANT_OPS:
                return None, f"{role if role != OBSERVER else 'an observer step'} runs {cmd[0]!r}"
    return roles, None


def check_invariants(root: Path, names: list, rcs: list) -> list:
    bad = []
    snap = snapshot(root)
    for name in names:
        if name == "rc_zero":
            if any(rcs):
                bad.append(f"rc_zero: {rcs}")
        elif name == "no_invalid_lines":
            for rel, v in snap.items():
                if rel.endswith(".jsonl") and any("$invalid" in e or "$no_eol" in e for e in v):
                    bad.append(f"no_invalid_lines: {rel}")
        elif name == "receipts_unique":
            for rel, v in snap.items():
                if rel.split("/")[-2:-1] == ["receipts"]:
                    keys = [(e.get("msg_id"), e.get("generation")) for e in v if "msg_id" in e]
                    if len(keys) != len(set(keys)):
                        bad.append(f"receipts_unique: {rel} {keys}")
        elif name == "journals_contiguous":
            for rel, v in snap.items():
                if rel.split("/")[-2:-1] == ["journal"]:
                    seqs = [e.get("seq") for e in v if "seq" in e]
                    if seqs != list(range(1, len(seqs) + 1)):
                        bad.append(f"journals_contiguous: {rel} {seqs}")
        else:
            bad.append(f"unknown invariant {name}")
    return bad


SUBCOMMAND = re.compile(r"^[a-z][a-z-]*$")


def check_argv(cmd: list) -> None:
    """Refuse a step whose arguments could redirect it away from the fixture
    root: it must start with a subcommand (so no global option precedes it), and
    no argument may be --repo or any prefix argparse would expand to it."""
    if not cmd or not isinstance(cmd, list) or not all(isinstance(x, str) for x in cmd):
        raise Refused("ARGV_REFUSED a step's arguments must be a non-empty list of strings")
    if not SUBCOMMAND.match(cmd[0]):
        raise Refused(f"ARGV_REFUSED {cmd[0]!r} is not a subcommand; nothing may precede it")
    for arg in cmd:
        flag = arg.split("=", 1)[0]
        if len(flag) > 2 and flag.startswith("--") and "--repo".startswith(flag):
            raise Refused(f"ARGV_REFUSED {arg!r}: the runner alone sets --repo")


def validate(fx: dict, capture: bool) -> None:
    """A fixture that claims a layer must carry that layer's checks."""
    layers = set(fx.get("layers", []))
    if not layers or not layers <= {"cli", "state"}:
        raise ValueError(f"{fx['id']}: layers must be a non-empty subset of cli, state")
    concurrent = False
    for i, st in enumerate(fx["steps"]):
        kinds = [k for k in ("cmd", "raw", "parallel") if k in st]
        if len(kinds) != 1:
            raise ValueError(f"{fx['id']} step {i}: exactly one of cmd, raw, parallel")
        if "cmd" in st:
            check_argv(st["cmd"])
            bad_env = set(st.get("env") or {}) - STEP_ENV
            if bad_env:
                raise Refused(f"ENV_REFUSED a step may not set {sorted(bad_env)}")
        if "parallel" in st:
            for cmd in st["parallel"]:
                check_argv(cmd)
            bad_env = set(st.get("env") or {}) - STEP_ENV
            if bad_env:
                raise Refused(f"ENV_REFUSED a step may not set {sorted(bad_env)}")
            # Concurrent output is nondeterministic; its state contract is the
            # invariants plus exact journal counts, and both are required.
            if not st.get("invariants") or not st.get("journal_counts"):
                raise ValueError(f"{fx['id']} step {i}: a parallel step needs invariants and journal_counts")
            concurrent = True
        elif "cmd" in st and concurrent and "state" in layers:
            # After concurrent writes the exact tree is not deterministic.
            raise ValueError(f"{fx['id']} step {i}: no exact state check may follow a parallel step")
        if capture or "cmd" not in st:
            continue
        missing = [k for k in ("rc", "stdout", "stderr") if k not in st]
        if "cli" in layers and missing:
            raise ValueError(f"{fx['id']} step {i}: cli layer but no expected {', '.join(missing)}")
        if "state" in layers and "state" not in st:
            raise ValueError(f"{fx['id']} step {i}: state layer but no expected state")
    if concurrent and "final_state" in fx and not capture:
        raise ValueError(f"{fx['id']}: a fixture with a parallel step has no exact final_state")
    if "state" in layers and not capture and not concurrent and "final_state" not in fx:
        raise ValueError(f"{fx['id']}: state layer but no final_state")


def run_fixture(fx: dict, impls: dict, capture: bool = False, obs: bool = False) -> list:
    """Failures as strings; empty means the fixture passed. With `capture`,
    the fixture's expected values are filled in place instead of checked.
    With `obs`, the CLI layer is checked by `obs_check` instead of exactly."""
    try:
        validate(fx, capture)
    except (ValueError, Refused) as e:
        return [str(e)]
    layers = set(fx["layers"])
    fails = []
    vars_ = {}
    root = new_root(fx.get("setup", {}).get("vcs", "git"))
    try:
        for i, st in enumerate(fx["steps"]):
            where = f"step {i}"
            if "raw" in st:
                op = dict(st["raw"])
                try:
                    op["path"] = resolve_session_path(root, op.get("path", ""))
                    raw_step(root, op)
                except Refused as e:
                    if st.get("expect_refused"):
                        continue
                    fails.append(f"{where}: {e}")
                    return fails
                if st.get("expect_refused"):
                    fails.append(f"{where}: raw step was not refused")
                continue
            if "parallel" in st:
                env = child_env(step_env(st, vars_))
                procs = [subprocess.Popen(argv_for(impls, root, cmd), stdout=subprocess.DEVNULL,
                                          stderr=subprocess.DEVNULL, env=env) for cmd in st["parallel"]]
                rcs = [p.wait(timeout=STEP_TIMEOUT) for p in procs]
                fails += [f"{where}: {b}" for b in check_invariants(root, st["invariants"], rcs)]
                snap = snapshot(root)
                for role, n in st["journal_counts"].items():
                    area, _, who = role.rpartition("/")
                    got = [len(v) for k, v in snap.items() if k.split("/")[-2:] == [area or "journal", f"{who}.jsonl"]]
                    if got != [n]:
                        fails.append(f"{where}: journal {role} has {got} records, expected {n}")
                continue
            p = invoke(impls, root, st["cmd"], step_env(st, vars_))
            for name, rx in (st.get("capture") or {}).items():
                m = re.search(rx, p.stdout)
                vars_[name] = m.group(1) if m else ""
            out, err = normalise_text(p.stdout, root), normalise_text(p.stderr, root)
            state = snapshot(root) if "state" in layers else None
            if capture:
                if "cli" in layers:
                    st.update(rc=p.returncode, stdout=out, stderr=err)
                if state is not None:
                    st["state"] = state
                continue
            where = f"step {i} {st['cmd'][:3]}"
            if "cli" in layers and obs:
                fails += [f"{where}: {b}" for b in obs_check(st, p.returncode, out, err)]
            elif "cli" in layers:
                if st["rc"] != p.returncode:
                    fails.append(f"{where}: rc {p.returncode} != {st['rc']}\n{err}")
                if st.get("compare") == "entries":
                    if entries(st["stdout"]) != entries(out) or len(st["stdout"]) != len(out):
                        fails.append(f"{where}: transcript entries differ")
                elif st.get("compare") == "blocks":
                    if blocks(st["stdout"]) != blocks(out):
                        fails.append(f"{where}: peer mail differs\n--- expected\n{st['stdout']}--- got\n{out}")
                elif st["stdout"] != out:
                    fails.append(f"{where}: stdout differs\n--- expected\n{st['stdout']}--- got\n{out}")
                if (last_line(st["stderr"]) != last_line(err) if st.get("stderr_compare") == "last_line"
                        else st["stderr"] != err):
                    fails.append(f"{where}: stderr differs\n--- expected\n{st['stderr']}--- got\n{err}")
            if state is not None and st["state"] != state:
                diff = sorted(k for k in set(st["state"]) | set(state) if st["state"].get(k) != state.get(k))
                fails.append(f"{where}: state differs in {diff}")
        if "state" in layers and not any("parallel" in st for st in fx["steps"]):
            got = snapshot(root)
            if capture:
                fx["final_state"] = got
            elif fx["final_state"] != got:
                exp = fx["final_state"]
                diff = sorted(k for k in set(exp) | set(got) if exp.get(k) != got.get(k))
                fails.append(f"final state differs in {diff}")
    finally:
        shutil.rmtree(root, ignore_errors=True)
    return fails


def load(path: Path) -> dict:
    fx = json.loads(path.read_text(encoding="utf-8"))
    for k in ("id", "profiles", "steps"):
        if k not in fx:
            raise ValueError(f"{path.name}: missing {k!r}")
    return fx


def fixtures(profile: str | None, names: list) -> list:
    paths = [Path(n) for n in names] if names else sorted(FIXTURES.glob("*.json"))
    out = []
    for p in paths:
        fx = load(p)
        if profile is None or profile in fx["profiles"]:
            out.append((p, fx))
    return out


def provenance(impls: dict) -> dict:
    """Which build a capture came from: the reviewed reference, never the build
    under test (a capture is checked in only after someone reads it)."""
    import hashlib
    ref = Path(impls["*"][-1])
    rev = subprocess.run(["git", "-C", str(ref.parent), "rev-parse", "--short=12", "HEAD"],
                         capture_output=True, text=True).stdout.strip()
    dirty = subprocess.run(["git", "-C", str(ref.parent), "status", "--porcelain", "--", ref.name],
                           capture_output=True, text=True).stdout.strip()
    return {"build": f"{rev}{'+dirty' if dirty else ''}:{ref.name}",
            "sha12": hashlib.sha256(ref.read_bytes()).hexdigest()[:12],
            "note": "captured from the reference and reviewed; never regenerate with a build under test"}


def parse_impls(pairs: list, reference: Path | None = None) -> dict:
    impls = {"*": [sys.executable, str(reference or REFERENCE)]}
    for item in pairs or []:
        role, _, cmd = item.partition("=")
        impls[role] = shlex.split(cmd, posix=os.name != "nt")
    return impls


def last_line(text: str) -> str:
    lines = [x for x in text.splitlines() if x.strip()]
    return lines[-1] if lines else ""


def legacy_view(fx: dict) -> dict:
    """The fixture as an older build should run it: each step's `legacy`
    expectation (where the behaviour deliberately changed) replaces the
    current one, and only the CLI layer is checked."""
    old = json.loads(json.dumps(fx))
    old["layers"] = ["cli"]
    old.pop("final_state", None)
    for st in old["steps"]:
        st.pop("state", None)
        st.update(st.pop("legacy", {}))
    return old


def capture_legacy(fx: dict, impls: dict, label: str) -> int:
    """Record, per step, how an older build behaves where it differs from the
    current expectation. Returns the number of differing steps."""
    old = json.loads(json.dumps(fx))
    old["layers"] = ["cli"]
    for st in old["steps"]:
        for k in ("rc", "stdout", "stderr", "state", "legacy"):
            st.pop(k, None)
    run_fixture(old, impls, capture=True)
    n = 0
    for st, was in zip(fx["steps"], old["steps"]):
        st.pop("legacy", None)
        if "cmd" not in st:
            continue
        same_out = (blocks(st["stdout"]) == blocks(was["stdout"]) if st.get("compare") == "blocks"
                    else entries(st["stdout"]) == entries(was["stdout"]) if st.get("compare") == "entries"
                    else st["stdout"] == was["stdout"])
        if st["rc"] != was["rc"] or not same_out or st["stderr"] != was["stderr"]:
            st["legacy"] = {"rc": was["rc"], "stdout": was["stdout"], "stderr": was["stderr"]}
            if "Traceback (most recent call last)" in was["stderr"]:
                # A crash's frames name the old build's path and line numbers;
                # only the exception itself is the behaviour worth recording.
                st["legacy"].update(stderr=last_line(was["stderr"]), stderr_compare="last_line")
            n += 1
    fx.pop("legacy_build", None)
    if n:
        fx["legacy_build"] = label
    return n


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("fixtures", nargs="*")
    ap.add_argument("--profile")
    ap.add_argument("--impl", action="append", help="ROLE=COMMAND; '*' for every role")
    ap.add_argument("--capture", action="store_true",
                    help="fill expected values from the reference and write the fixture back")
    ap.add_argument("--capture-legacy", metavar="OLD_PAIR_PY",
                    help="record where an older build differs, as each step's `legacy` block")
    ap.add_argument("--legacy-label", default="", help="the older build's name, stored as legacy_build")
    ap.add_argument("--check-legacy", metavar="OLD_PAIR_PY",
                    help="run the fixtures' legacy view (CLI only) against an older build")
    ap.add_argument("--reference", type=Path, help="the reference pair.py (default: the skill's, or a vendored copy's)")
    ap.add_argument("--participant", action="append", metavar="ROLE=COMMAND",
                    help="participant profile: that role's steps, and steps naming no role, run on COMMAND; "
                         "checked on the STATE and obs layers; prints selected/total/skipped")
    a = ap.parse_args(argv)
    if a.reference and not a.reference.is_file():
        print(f"no reference implementation at {a.reference}", file=sys.stderr)
        return 2
    if a.participant:
        return run_participant(a)
    impls = parse_impls(a.impl, a.reference)
    failed = 0
    for path, fx in fixtures(a.profile, a.fixtures):
        if a.capture_legacy:
            n = capture_legacy(fx, {"*": [sys.executable, a.capture_legacy]}, a.legacy_label or a.capture_legacy)
            path.write_text(json.dumps(fx, indent=1, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
            print(f"LEGACY {fx['id']} differs_in={n}")
            continue
        if a.check_legacy:
            fx = legacy_view(fx)
            impls = {"*": [sys.executable, a.check_legacy]}
        if a.capture:
            run_fixture(fx, impls, capture=True)
            fx["provenance"] = provenance(impls)
            path.write_text(json.dumps(fx, indent=1, ensure_ascii=False) + "\n", encoding="utf-8", newline="\n")
            print(f"CAPTURED {fx['id']}")
            continue
        try:
            fails = run_fixture(fx, impls)
        except Refused as e:
            fails = [str(e)]
        if fails:
            failed += 1
            print(f"FAIL {fx['id']}")
            for f in fails:
                print("  " + f.replace("\n", "\n  "))
        else:
            print(f"PASS {fx['id']}")
    return 1 if failed else 0


def run_participant(a) -> int:
    """The participant-profile run: select, report every skip, enforce the
    mandatory list, and check STATE plus obs."""
    native = parse_impls(a.participant, a.reference)
    roles = [r for r in native if r != "*"]
    mandatory = set(json.loads(MANDATORY_FILE.read_text(encoding="utf-8"))["mandatory"])
    selected, skipped, failed = 0, [], 0
    all_fx = fixtures(a.profile, a.fixtures)
    for path, fx in all_fx:
        use, why = participant_selection(fx, roles)
        if use is None:
            skipped.append((fx["id"], why))
            continue
        selected += 1
        impls = {"*": native["*"], OBSERVER: native[use[0]], **{r: native[r] for r in use}}
        try:
            fails = run_fixture(fx, impls, obs=True)
        except Refused as e:
            fails = [str(e)]
        if fails:
            failed += 1
            print(f"FAIL {fx['id']} (native: {','.join(use)})")
            for f in fails:
                print("  " + f.replace("\n", "\n  "))
        else:
            print(f"PASS {fx['id']} (native: {','.join(use)})")
    for fid, why in skipped:
        print(f"SKIP {fid}: {why}")
    ran = {fx["id"] for _, fx in all_fx}
    missing = sorted(m for m in mandatory if m in {i for i, _ in skipped} or (not a.fixtures and m not in ran))
    for m in missing:
        print(f"MANDATORY_NOT_RUN {m}")
    print(f"SELECTED {selected} / TOTAL {len(all_fx)} / SKIPPED {len(skipped)} / FAILED {failed}")
    return 1 if failed or missing else 0


if __name__ == "__main__":
    sys.exit(main())
