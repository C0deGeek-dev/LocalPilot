#!/usr/bin/env python3
"""Mixed-writer soak: two implementations writing one live mailbox at once.

    python soak.py                                   # the reference against itself
    python soak.py --impl-b "<cmd>" --posts 200      # the reference against <cmd>

Two sessions run in turn in a scratch repository: a historic pair (claude,
codex) and a three-party session (claude, codex, localpilot). Every role has
two posters and two readers running concurrently, one of each on each
implementation, so each role's own journal, latest record and cursor are
written by both implementations under the shared locks. In the three-party
session another thread exercises the delivery facts (endpoint, accept,
record-push) across both, and `status` runs throughout.

A bounded "mailbox lock busy" is retried a few times and counted; an operation
that still fails, or fails any other way, fails the run.

Afterwards an oracle checks the mailbox: no invalid journal line, contiguous
sequence numbers, every successful post present exactly once, every message
delivered to and acknowledged by each of its recipients, cursors within the
journals, unique receipts. The run ends with `SOAK ok ...` or
`SOAK FAIL <check>: <detail>` and exit 1.
"""
from __future__ import annotations

import argparse, collections, json, os, random, re, shlex, shutil, subprocess, sys, threading, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
# Importing the runner must not leave `__pycache__` beside it: in a vendored
# copy that is an unlisted file, and the manifest check reports it as drift.
sys.dont_write_bytecode = True
import run  # noqa: E402  (the runner's containment and reference lookup)

CMD_TIMEOUT = 120  # seconds; --cmd-timeout overrides
BUSY = "mailbox lock busy"
BUSY_RETRIES = 5
FACT_ROUNDS = 3          # accept + record-push rounds each three-party session must complete
FACT_DRAIN_TRIES = 200   # 50 ms looks after the posters finish before giving up on them
PEER = re.compile(r"^PEER ([a-z]+) #(\d+) ", re.M)
TOKEN = re.compile(r"soak:[a-z]+:s\d+:[A-Z]+:\d+")
ODD = ["", " caf\u00e9", " line\u2028sep", " para\u2029sep", " \u0085nel", " emoji \U0001F600"]


class Soak:
    def __init__(self, impls: dict, root: Path, rnd: random.Random, cmd_timeout: float = CMD_TIMEOUT):
        self.impls, self.root, self.rnd, self.cmd_timeout = impls, root, rnd, cmd_timeout
        self.session_no = 0
        self.accepted: set = set()   # this session: (msg_id, generation) of every accept that returned 0
        self.pushed = collections.Counter()  # this session: (msg_id, to, generation, outcome) per push that returned 0
        self.lock = threading.Lock()
        self.ops = 0
        self.busy = 0
        self.retries = 0
        self.errors: list = []
        self.posts_total = 0
        self.posted: dict = {}      # this session: token -> role, for every post that returned 0
        self.delivered: dict = {}   # this session: reader -> set of (sender, seq)

    # --- commands --------------------------------------------------------------

    def call(self, impl: str, args: list, env: dict | None = None) -> subprocess.CompletedProcess:
        e = run.child_env(env)
        for attempt in range(BUSY_RETRIES + 1):
            try:
                p = subprocess.run([*self.impls[impl], "--repo", str(self.root), *args], text=True,
                                   encoding="utf-8", errors="replace", stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, env=e, timeout=self.cmd_timeout)
            except (OSError, subprocess.TimeoutExpired) as x:
                # A command that hangs or cannot start is a failed operation,
                # never a silently shorter run.
                p = subprocess.CompletedProcess(args, 124, "", f"{type(x).__name__}: {x}")
            busy = p.returncode != 0 and BUSY in p.stderr
            with self.lock:
                self.ops += 1
                self.busy += busy
                self.retries += busy and attempt < BUSY_RETRIES
            if not busy:
                break
            time.sleep(0.1 * (attempt + 1))
        if p.returncode:
            with self.lock:
                self.errors.append(f"{impl} {' '.join(args[:3])}: rc {p.returncode}: {p.stderr.strip()[-400:]}")
        return p

    def session_dir(self) -> Path:
        base = self.root / run.MAILBOX
        a = base / "active.v2.json"
        if not a.exists():
            a = base / "active.json"
        return base / "sessions" / json.loads(a.read_text(encoding="utf-8"))["session_id"]

    def journal(self, sd: Path, role: str) -> list:
        p = sd / "journal" / f"{role}.jsonl"
        out = []
        if not p.exists():
            return out
        for line in p.read_bytes().split(b"\n"):
            try:
                m = json.loads(line.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if isinstance(m, dict):
                out.append(m)
        return out

    # --- actors ----------------------------------------------------------------

    def actor(self, target, *args) -> threading.Thread:
        """A thread whose exception fails the run instead of ending it early."""
        def body():
            try:
                target(*args)
            except BaseException as x:  # noqa: BLE001 - any crash is a failure
                with self.lock:
                    self.errors.append(f"worker {target.__name__} crashed: {type(x).__name__}: {x}")
        return threading.Thread(target=body)

    def poster(self, impl: str, role: str, parts: list, posts: int, schema2: bool, sd: Path) -> None:
        others = [r for r in parts if r != role]
        for n in range(posts):
            token = f"soak:{role}:s{self.session_no}:{impl}:{n}"
            body = token + self.rnd.choice(ODD)
            args = ["post", "--role", role, "--body", body]
            pick = self.rnd.random()
            if schema2:
                inbox = [m for r in others for m in self.journal(sd, r)
                         if role in (m.get("to") or []) and m.get("msg_id")]
                if pick < 0.3 and inbox:
                    args += ["--kind", "ANSWER", "--reply-to", self.rnd.choice(inbox)["msg_id"]]
                elif pick < 0.35:
                    args += ["--kind", "ESCALATE", "--broadcast"]
                else:
                    to = self.rnd.sample(others, self.rnd.randint(1, len(others)))
                    args += ["--kind", self.rnd.choice(["NOTE", "QUESTION"]), "--to", ",".join(to)]
                    if pick > 0.8:
                        args.append("--expect-reply")
            else:
                args += ["--kind", self.rnd.choice(["NOTE", "QUESTION", "ANSWER"])]
                if pick > 0.8:
                    args.append("--expect-reply")
            self.record_post(token, role, self.call(impl, args).returncode)

    def record_post(self, token: str, role: str, rc: int) -> None:
        if rc:
            return
        with self.lock:
            if token in self.posted:
                self.errors.append(f"duplicate token {token}: the soak generated it twice")
            self.posted[token] = role
            self.posts_total += 1

    def read_once(self, impl: str, role: str, schema2: bool) -> int:
        # The acknowledgement goes through the other implementation half the
        # time, so both write this reader's cursor.
        ack_impl = impl if self.rnd.random() < 0.5 else ("B" if impl == "A" else "A")
        p = self.call(impl, ["peek", "--role", role])
        seen = [(s, int(n)) for s, n in PEER.findall(p.stdout)]
        if not seen:
            return 0
        with self.lock:
            self.delivered.setdefault(role, set()).update(seen)
        top: dict = {}
        for s, n in seen:
            top[s] = max(top.get(s, 0), n)
        through = ",".join(f"{s}:{n}" for s, n in sorted(top.items())) if schema2 else str(max(top.values()))
        self.call(ack_impl, ["ack", "--role", role, "--through", through])
        return len(seen)

    def reader(self, impl: str, role: str, schema2: bool, stop: threading.Event) -> None:
        while not stop.is_set():
            if not self.read_once(impl, role, schema2):
                time.sleep(0.05)
        idle = 0
        while idle < 2:  # drain: two empty looks in a row
            idle = idle + 1 if not self.read_once(impl, role, schema2) else 0

    def facts(self, sd: Path, stop: threading.Event) -> None:
        # One registration (a second live one without its token is refused, by
        # design); accepts and push records then alternate implementations.
        other = lambda: self.rnd.choice(["A", "B"])
        p = self.call(other(), ["endpoint", "--role", "localpilot", "--register", "--transport", "soak",
                                "--address", "soak://localpilot", "--ttl", "3600"])
        tok = re.search(r"ENDPOINT_TOKEN=([0-9a-f]{64})", p.stdout)
        gen = re.search(r"generation=(\d+)", p.stdout)
        if not (tok and gen):
            with self.lock:
                self.errors.append(f"endpoint register printed no token: {p.stdout!r} {p.stderr!r}")
            return
        env = {"PAIR_ENDPOINT_TOKEN": tok.group(1)}
        g = int(gen.group(1))
        # One message of its own, so every run exercises the facts however few
        # posts the posters make.
        token = f"soak:claude:s{self.session_no}:FACTS:0"
        self.record_post(token, "claude", self.call(other(), ["post", "--role", "claude", "--kind", "NOTE",
                                                              "--to", "localpilot", "--body", token]).returncode)
        rounds = tries = 0
        # After the posters finish, keep going only until the required rounds
        # are done or a bounded number of looks: a run with nothing to accept
        # must fail, not hang.
        while not stop.is_set() or (rounds < FACT_ROUNDS and tries < FACT_DRAIN_TRIES):
            tries += stop.is_set()
            mine = [m for m in self.journal(sd, "claude") if "localpilot" in (m.get("to") or []) and m.get("msg_id")]
            if mine:
                m = self.rnd.choice(mine)
                if self.call(other(), ["accept", "--role", "localpilot", "--msg-id", m["msg_id"],
                                       "--generation", str(g)], env=env).returncode == 0:
                    with self.lock:
                        self.accepted.add((m["msg_id"], g))
                outcome = self.rnd.choice(["sent", "refused", "failed", "timeout"])
                if self.call(other(), ["record-push", "--role", "claude", "--msg-id", m["msg_id"], "--to", "localpilot",
                                       "--generation", str(g), "--outcome", outcome]).returncode == 0:
                    with self.lock:
                        self.pushed[(m["msg_id"], "localpilot", g, outcome)] += 1
                rounds += 1
            time.sleep(0.05)
        if rounds < FACT_ROUNDS:
            with self.lock:
                self.errors.append(f"facts: only {rounds} of {FACT_ROUNDS} accept/push rounds ran; "
                                   "no message addressed to localpilot was available")

    def watcher(self, impl: str, stop: threading.Event) -> None:
        while not stop.is_set():
            self.call(impl, ["status"])
            time.sleep(0.2)

    # --- one session -----------------------------------------------------------

    def session(self, parts: list, posts: int) -> None:
        schema2 = len(parts) > 2
        self.session_no += 1
        self.posted, self.delivered, self.accepted, self.pushed = {}, {}, set(), collections.Counter()
        start = ["start", "--role", "claude", "--task", f"soak {len(parts)}"]
        if schema2:
            start += ["--with", ",".join(parts[1:])]
        self.call("A", start)
        for i, r in enumerate(parts[1:], 1):
            self.call("B" if i % 2 else "A", ["join", "--role", r, "--timeout", "10"])
        sd = self.session_dir()
        stop = threading.Event()
        posters, others = [], []
        for r in parts:
            for impl in ("A", "B"):
                posters.append(self.actor(self.poster, impl, r, parts, posts, schema2, sd))
                others.append(self.actor(self.reader, impl, r, schema2, stop))
        if schema2:
            others.append(self.actor(self.facts, sd, stop))
        others += [self.actor(self.watcher, impl, stop) for impl in ("A", "B")]
        for t in posters + others:
            t.start()
        for t in posters:
            t.join()
        stop.set()
        for t in others:
            t.join()
        for impl in ("A", "B"):  # the session and its pointers still read cleanly
            self.call(impl, ["status"])
        # Every intended post must have succeeded: a worker that stopped early,
        # or a post that failed, is a shorter run, not a clean one.
        want = posts * 2 * len(parts) + (1 if schema2 else 0)
        if len(self.posted) != want:
            self.fail("posts", f"{len(self.posted)} of {want} intended posts succeeded")
        self.check(sd, parts, schema2)
        self.call("A", ["abandon", "--role", "claude", "--reason", "soak done"])

    # --- oracle ----------------------------------------------------------------

    def fail(self, check: str, detail: str) -> None:
        self.errors.append(f"{check}: {detail}")

    def check(self, sd: Path, parts: list, schema2: bool) -> None:
        journals = {}
        for r in parts:
            p = sd / "journal" / f"{r}.jsonl"
            raw = p.read_bytes() if p.exists() else b""
            if raw and not raw.endswith(b"\n"):
                self.fail("torn_tail", f"{r}.jsonl does not end in a newline")
            lines = [x for x in raw.split(b"\n") if x]
            recs = self.journal(sd, r)
            if len(recs) != len(lines):
                self.fail("invalid_lines", f"{r}.jsonl has {len(lines) - len(recs)} invalid line(s)")
            seqs = [int(m.get("seq", 0)) for m in recs]
            if seqs != list(range(1, len(seqs) + 1)):
                self.fail("contiguous", f"{r}.jsonl seqs are not 1..{len(seqs)}")
            latest = sd / "latest" / f"{r}.json"
            if recs and (not latest.exists() or json.loads(latest.read_text(encoding="utf-8")).get("seq") != seqs[-1]):
                self.fail("latest", f"latest/{r}.json does not name seq {seqs[-1]}")
            journals[r] = recs
        found: dict = {}
        for r, recs in journals.items():
            for m in recs:
                for t in TOKEN.findall(m.get("body") or ""):
                    found.setdefault(t, []).append(r)
        mine = set(self.posted)
        lost = sorted(mine - set(found))
        dup = sorted(t for t, rs in found.items() if len(rs) > 1)
        if lost:
            self.fail("lost", f"{len(lost)} successful post(s) missing, e.g. {lost[:3]}")
        if dup:
            self.fail("duplicate", f"{len(dup)} token(s) written more than once, e.g. {dup[:3]}")
        for r in parts:
            want = set()
            for s, recs in journals.items():
                if s == r:
                    continue
                for m in recs:
                    if not schema2 or r in (m.get("to") or []) or m.get("broadcast"):
                        want.add((s, int(m["seq"])))
            missing = sorted(want - self.delivered.get(r, set()))
            if missing:
                self.fail("undelivered", f"{r} never saw {len(missing)} message(s), e.g. {missing[:3]}")
            cur = sd / "cursor" / f"{r}.json"
            c = json.loads(cur.read_text(encoding="utf-8")) if cur.exists() else {}
            per = c.get("from") if schema2 else {s: c for s in parts if s != r}
            for s, st in (per or {}).items():
                top = len(journals.get(s, []))
                ack, dl = int(st.get("peer_seq", 0)), int(st.get("delivered_seq", 0))
                if ack > top:
                    self.fail("cursor", f"{r} acked {s}:{ack} beyond its journal ({top})")
                if dl > ack:
                    self.fail("unacked", f"{r} still has {s}:{ack + 1}..{dl} unacknowledged")
        if schema2:
            self.check_facts(sd, journals)

    def check_facts(self, sd: Path, journals: dict) -> None:
        """Every successful accept left exactly one receipt, and every
        successful push record a line; each names a message addressed to
        localpilot."""
        if not self.accepted or not self.pushed:
            self.fail("facts", f"too few facts exercised (accepts={len(self.accepted)}, pushes={sum(self.pushed.values())})")
        to_lp = {m["msg_id"] for recs in journals.values() for m in recs
                 if m.get("msg_id") and ("localpilot" in (m.get("to") or []) or m.get("broadcast"))}
        receipts = self.journal(sd, "../receipts/localpilot")
        keys = [(m.get("msg_id"), m.get("generation")) for m in receipts]
        if len(keys) != len(set(keys)):
            self.fail("receipts_unique", "a (msg_id, generation) has more than one receipt")
        if set(keys) != self.accepted:
            self.fail("receipts", f"{len(self.accepted - set(keys))} accepted message(s) have no receipt; "
                                  f"{len(set(keys) - self.accepted)} receipt(s) match no accept")
        stray = [k for k, _ in keys if k not in to_lp]
        if stray:
            self.fail("receipts", f"receipt for a message not addressed to localpilot: {stray[:3]}")
        pushes = self.journal(sd, "../pushes/claude")
        rows = collections.Counter((m.get("msg_id"), m.get("to"), m.get("generation"), m.get("outcome")) for m in pushes)
        if rows != self.pushed:
            self.fail("pushes", f"push records differ from the successful calls: missing {dict(self.pushed - rows)}, "
                                f"unexpected {dict(rows - self.pushed)}")
        stray = [m.get("msg_id") for m in pushes if m.get("msg_id") not in to_lp or m.get("to") != "localpilot"]
        if stray:
            self.fail("pushes", f"push record for a message not addressed to localpilot: {stray[:3]}")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--impl-a", help="implementation A (default: the reference)")
    ap.add_argument("--impl-b", help="implementation B (default: the reference)")
    ap.add_argument("--posts", type=int, default=200, help="posts per poster per session")
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--keep", action="store_true", help="keep the scratch repository")
    ap.add_argument("--cmd-timeout", type=float, default=CMD_TIMEOUT, help="seconds before one command counts as hung")
    a = ap.parse_args(argv)
    # Error lines quote bodies holding non-BMP and separator characters; a
    # console in a legacy code page must not turn a failure report into a crash.
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="backslashreplace")
    ref = [sys.executable, str(run.REFERENCE)]
    split = lambda c: shlex.split(c, posix=os.name != "nt") if c else ref
    impls = {"A": split(a.impl_a), "B": split(a.impl_b)}
    seed = a.seed if a.seed is not None else random.randrange(10**9)
    root = run.new_root("git")
    s = Soak(impls, root, random.Random(seed), a.cmd_timeout)
    began = time.monotonic()
    try:
        for parts in (["claude", "codex"], ["claude", "codex", "localpilot"]):
            s.session(parts, a.posts)
    finally:
        if not a.keep:
            shutil.rmtree(root, ignore_errors=True)
    elapsed = time.monotonic() - began
    if s.errors:
        print(f"SOAK FAIL seed={seed} " + s.errors[0])
        for e in s.errors[1:20]:
            print("  " + e)
        return 1
    print(f"SOAK ok seed={seed} ops={s.ops} posts={s.posts_total} busy={s.busy} retried={s.retries} elapsed={elapsed:.0f}s"
          + (f" kept={root}" if a.keep else ""))
    return 0


if __name__ == "__main__":
    sys.exit(main())
