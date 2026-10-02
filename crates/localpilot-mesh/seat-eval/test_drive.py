"""The driver's own safety: it deletes only what it made, never runs on
changed fixtures, and never leaves an engine running. No model is needed.

    python -m unittest test_drive      (from this directory)
"""

import argparse
import importlib.util
import json
import os
import pathlib
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

HERE = pathlib.Path(__file__).resolve().parent


def load(root):
    """The driver module as found under `root`."""
    spec = importlib.util.spec_from_file_location(f"drive_{id(root)}", root / "drive.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def args(out, **kw):
    a = dict(model="m", label="t", cell="owner", task="roman", run=1, out=str(out),
             wall=30, localpilot=sys.executable, provider=None, context_window=None,
             review_case="roman-v1", review_diagnostics=False)
    a.update(kw)
    return argparse.Namespace(**a)


class RunPathTest(unittest.TestCase):
    def test_a_label_or_run_that_escapes_the_results_directory_is_refused(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d).resolve() / "results"
            victim = pathlib.Path(d) / "outside-owner-roman-1"
            victim.mkdir()
            (victim / "keep.txt").write_text("keep")
            for label in ("../../outside", "../outside", "a/b", "a\\b", ".hidden", "", "x" * 41):
                with self.assertRaises(SystemExit, msg=label):
                    drive.run_path(out, args(out, label=label))
            with self.assertRaises(SystemExit):
                drive.run_path(out, args(out, run=0))
            self.assertTrue((victim / "keep.txt").is_file())
            self.assertFalse(out.exists(), "nothing is created for a refused name")

    def test_an_existing_directory_is_replaced_only_if_the_driver_made_it(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            unmarked = pathlib.Path(d) / "someone-elses"
            unmarked.mkdir()
            (unmarked / "keep.txt").write_text("keep")
            with self.assertRaises(SystemExit):
                drive.scratch(unmarked, "spec")
            self.assertEqual((unmarked / "keep.txt").read_text(), "keep")
            made = pathlib.Path(d) / "ours"
            drive.scratch(made, "spec")
            (made / "leftover.txt").write_text("old run")
            drive.scratch(made, "spec")
            self.assertFalse((made / "leftover.txt").exists(), "a prior scratch run is replaced")
            self.assertTrue((made / drive.MARKER).is_file())


class RerunTest(unittest.TestCase):
    def test_a_used_run_name_is_refused_and_its_evidence_kept(self):
        drive = load(HERE)
        started = []
        drive.engine = lambda *a, **k: started.append(a)
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d).resolve() / "results"
            name = "t-owner-roman-1"
            for leave in ("repo", "log", "row", "diagnostic"):
                shutil.rmtree(out, ignore_errors=True)
                out.mkdir()
                if leave == "repo":
                    (out / name).mkdir()
                    (out / name / drive.MARKER).write_text("x")
                    (out / name / "evidence.txt").write_text("first run")
                elif leave == "log":
                    (out / f"{name}.log").write_text("first run log")
                elif leave == "diagnostic":
                    (out / f"{name}.review.jsonl").write_text("first capture")
                else:
                    (out / "results.jsonl").write_text(json.dumps({"name": name, "hidden_ok": True}) + "\n")
                before = sorted((p.relative_to(out).as_posix(), p.read_bytes()) for p in out.rglob("*") if p.is_file())
                with self.assertRaises(SystemExit, msg=leave) as refused:
                    drive.run(args(out))
                self.assertIn("already exists", str(refused.exception))
                after = sorted((p.relative_to(out).as_posix(), p.read_bytes()) for p in out.rglob("*") if p.is_file())
                self.assertEqual(before, after, leave)
            self.assertEqual(started, [])


class RemoveTreeTest(unittest.TestCase):
    def test_an_entry_that_vanishes_during_the_delete_is_not_an_error(self):
        # Git's background maintenance can remove its own lock file while the
        # tree is being deleted; that entry is already gone.
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            tree = pathlib.Path(d) / "run"
            (tree / "objects").mkdir(parents=True)
            gone = tree / "objects" / "maintenance.lock"
            gone.write_text("x")
            real = drive.shutil.rmtree

            def rmtree_with_a_vanishing_entry(path, **kw):
                handler = kw.get("onexc") or kw.get("onerror")
                gone.unlink()
                handler(os.unlink, str(gone), None)
                return real(path, **kw)

            drive.shutil.rmtree = rmtree_with_a_vanishing_entry
            try:
                drive.remove_tree(tree)
            finally:
                drive.shutil.rmtree = real
            self.assertFalse(tree.exists())

    def test_a_scratch_repository_runs_no_background_maintenance(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            repo = pathlib.Path(d) / "run"
            drive.scratch(repo, "spec")
            conf = subprocess.run(["git", "-C", str(repo), "config", "--get", "maintenance.auto"],
                                  capture_output=True, text=True).stdout.strip()
            self.assertEqual(conf, "false")


class FixtureGuardTest(unittest.TestCase):
    def test_a_run_on_changed_fixtures_is_refused_before_any_engine_starts(self):
        with tempfile.TemporaryDirectory() as d:
            copy = pathlib.Path(d) / "seat-eval"
            shutil.copytree(HERE, copy, ignore=shutil.ignore_patterns("__pycache__"))
            spec = copy / "tasks" / "roman" / "spec.md"
            spec.write_text(spec.read_text(encoding="utf-8") + "\nOne more rule.\n", encoding="utf-8")
            drive = load(copy)
            started = []
            drive.engine = lambda *a, **k: started.append(a)
            out = pathlib.Path(d) / "results"
            with self.assertRaises(SystemExit) as refused:
                drive.run(args(out))
            self.assertIn("changed fixtures", str(refused.exception))
            self.assertEqual(started, [])
            self.assertFalse(out.exists())


class ReviewCaseTest(unittest.TestCase):
    def test_versioned_run_names_keep_legacy_identity_and_refuse_unknown_cases(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            legacy, _ = drive.run_path(out, args(out, cell="review-good"))
            revised, _ = drive.run_path(out, args(out, cell="review-good", review_case="roman-v2"))
            self.assertEqual(legacy, "t-review-good-1")
            self.assertEqual(revised, "t-review-good-roman-v2-1")
            for case in ("unknown", "../outside", "roman-v2/../x"):
                with self.assertRaises(SystemExit):
                    drive.run(args(out, cell="review-good", review_case=case))
            with self.assertRaises(SystemExit):
                drive.run(args(out, review_case="roman-v2"))
            self.assertFalse(out.exists())

    def test_v2_visible_tests_pin_bool_rejection_and_the_planted_case_stays_frozen(self):
        drive = load(HERE)
        clean = drive.REVIEW_CASES["roman-v2"]["clean"]
        planted = drive.REVIEW_CASES["roman-v2"]["planted"]
        self.assertEqual(planted, drive.REVIEW_CASES["roman-v1"]["planted"])
        with tempfile.TemporaryDirectory() as d:
            repo = pathlib.Path(d)
            shutil.copyfile(clean / "test_roman.py", repo / "test_roman.py")
            for implementation, expect in ((clean, 0), (planted, 1)):
                shutil.copyfile(implementation / "roman.py", repo / "roman.py")
                # No stale bytecode when two same-sized implementations are swapped.
                shutil.rmtree(repo / "__pycache__", ignore_errors=True)
                result = drive.run_cmd([sys.executable, "-B", "-m", "unittest", "-v"],
                                       repo, check=False)
                self.assertEqual(result.returncode, expect, result.stdout + result.stderr)
                if expect:
                    self.assertIn("test_non_integers", result.stderr)
                    self.assertIn("ValueError not raised", result.stderr)

    def test_real_driver_selects_and_records_v2_without_changing_raw_scoring(self):
        drive = load(HERE)
        drive.engine = lambda *a, **k: subprocess.Popen([sys.executable, "-c", "pass"])
        drive.journal = lambda *a: [{"kind": "VERDICT", "body": "REVISE round=1 blocking=1 important=0"}]
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            for cell, source, expected in (("review-good", "clean", "AGREE"),
                                           ("review-bad", "planted", "REVISE")):
                drive.run(args(out, cell=cell, review_case="roman-v2"))
                row = json.loads((out / "results.jsonl").read_text().splitlines()[-1])
                self.assertEqual(row["review_case"], "roman-v2")
                self.assertEqual(row["expected"], expected)
                self.assertEqual(row["false_revise"], cell == "review-good")
                self.assertFalse(row["false_agree"])
                self.assertEqual(row["review_spec_hash"], drive.sha(drive.TASKS / "roman" / "spec.md"))
                repo = out / row["name"]
                selected = drive.REVIEW_CASES["roman-v2"][source]
                for file in drive.REVIEW_FILES:
                    self.assertEqual((repo / file).read_bytes(), (selected / file).read_bytes())
                    self.assertEqual(row["review_fixture_hashes"][file], drive.sha(selected / file))


class ChildCleanupTest(unittest.TestCase):
    def test_a_driver_failure_after_the_engine_starts_stops_it_and_is_recorded(self):
        drive = load(HERE)
        spawned = []

        def fake_engine(a, repo, extra, log):
            p = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
            spawned.append(p)
            return p

        def broken_journal(repo, role):
            raise RuntimeError("injected failure")

        drive.engine = fake_engine
        drive.journal = broken_journal
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            with self.assertRaises(RuntimeError):
                drive.run(args(out))
            self.assertEqual(len(spawned), 1)
            self.assertIsNotNone(spawned[0].poll(), "the engine was left running")
            rows = [json.loads(l) for l in (out / "results.jsonl").read_text(encoding="utf-8").splitlines()]
            self.assertEqual(len(rows), 1)
            self.assertIn("injected failure", rows[0]["driver_error"])
            self.assertEqual(rows[0].get("review_case"), None)

    def test_a_failed_v2_review_keeps_its_fixture_identity(self):
        drive = load(HERE)
        def broken_review(*a):
            raise RuntimeError("injected review failure")
        drive.review_cell = broken_review
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            with self.assertRaises(RuntimeError):
                drive.run(args(out, cell="review-bad", review_case="roman-v2"))
            row = json.loads((out / "results.jsonl").read_text())
            self.assertEqual(row["review_case"], "roman-v2")
            planted = drive.REVIEW_CASES["roman-v2"]["planted"]
            self.assertEqual(row["review_fixture_hashes"]["roman.py"], drive.sha(planted / "roman.py"))


class OwnerJournalTest(unittest.TestCase):
    def owner(self, messages, *, live=False, exit_during=None, status="active",
              session_name="session.v2.json", ambiguous=False):
        drive = load(HERE)
        calls = []
        measurements = []
        children = []
        with tempfile.TemporaryDirectory() as d:
            repo = pathlib.Path(d) / "repo"
            drive.scratch = lambda *a: repo.mkdir()
            session = repo / ".pair-programming" / "sessions" / "control"
            record = session / session_name

            def fake_engine(*a):
                session.mkdir(parents=True)
                if status is not None:
                    record.write_text(json.dumps({"status": status}))
                if ambiguous:
                    (session / "session.json").write_text('{"status":"completed"}')
                path = session / "journal" / "localpilot.jsonl"
                path.parent.mkdir()
                if live:
                    path.write_text("".join(json.dumps(m) + "\n" for m in messages))
                    proc = mock.Mock(returncode=None)
                    proc.poll.side_effect = lambda: proc.returncode
                    proc.wait.side_effect = lambda **kw: proc.returncode
                else:
                    # A real child writes its final journal and exits before the
                    # next poll; exercise owner_cell rather than a parser alone.
                    script = ("import pathlib,sys; "
                              "pathlib.Path(sys.argv[1]).write_text(sys.argv[2], encoding='utf-8')")
                    proc = subprocess.Popen([sys.executable, "-c", script, str(path),
                                             "".join(json.dumps(m) + "\n" for m in (messages or []))])
                    proc.wait(timeout=10)
                    if messages is None:
                        path.unlink()
                children.append(proc)
                return proc

            def fake_pair(repo, *argv, **kw):
                calls.append(argv)
                if argv[0] == "post":
                    children[0].returncode = 0
                    record.write_text('{"status":"completed"}')
                if argv[0] == exit_during:
                    children[0].returncode = 0
                return mock.Mock(stdout="localpilot: ready", returncode=0)

            def fake_hidden(*a):
                measurements.append(children[0].poll())
                if exit_during == "hidden":
                    children[0].returncode = 0
                return True, "HIDDEN_OK"

            drive.engine = fake_engine
            drive.pair = fake_pair
            drive.hidden = fake_hidden
            with mock.patch.object(drive.time, "sleep"):
                row = drive.owner_cell(args(pathlib.Path(d)), repo, None)
            self.assertIsNotNone(children[0].poll())
        return row, calls, measurements

    def test_final_escalation_is_reconciled_after_real_child_exit(self):
        row, calls, measured = self.owner([{"kind": "ESCALATE"}])
        self.assertTrue(row["escalated"])
        self.assertFalse(row["review_requested"])
        self.assertFalse(row["protocol_completed"])
        self.assertEqual(row["engine_exit"], 0)
        self.assertEqual(row["hidden_measure"], "final_tree_after_exit")
        self.assertEqual(measured, [0])
        self.assertFalse(any(c[0] in ("watch", "post") for c in calls))

    def test_late_request_is_observed_without_agreement_or_snapshot_claim(self):
        row, calls, measured = self.owner([{"kind": "REVIEW_REQUEST", "seq": 3}])
        self.assertTrue(row["review_requested"])
        self.assertEqual(row["review_observation"], "after_exit")
        self.assertEqual(row["review_request_id"], "localpilot:3")
        self.assertNotIn("request_at_s", row)
        self.assertEqual(row["hidden_measure"], "final_tree_after_exit")
        self.assertFalse(row["protocol_completed"])
        self.assertEqual(measured, [0])
        self.assertFalse(any(c[0] in ("watch", "post") for c in calls))

    def test_live_request_keeps_one_measurement_and_actual_completion(self):
        row, calls, measured = self.owner(
            [{"kind": "REVIEW_REQUEST", "msg_id": "localpilot:3"}], live=True)
        self.assertTrue(row["review_requested"])
        self.assertEqual(row["review_observation"], "live")
        self.assertEqual(row["hidden_measure"], "first_request_observed_tree")
        self.assertIn("request_at_s", row)
        self.assertTrue(row["protocol_completed"])
        self.assertEqual(measured, [None])
        posts = [c for c in calls if c[0] == "post"]
        self.assertEqual(len(posts), 1)
        self.assertEqual(posts[0][posts[0].index("--reply-to") + 1], "localpilot:3")

    def test_exit_during_hidden_or_watch_never_posts_agreement(self):
        for stage in ("hidden", "watch"):
            with self.subTest(stage=stage):
                row, calls, measured = self.owner(
                    [{"kind": "REVIEW_REQUEST", "seq": 3}], live=True, exit_during=stage)
                self.assertFalse(any(c[0] == "post" for c in calls))
                self.assertFalse(row["protocol_completed"])
                self.assertEqual(measured, [None])
                self.assertEqual(row["hidden_measure"], "first_request_observed_tree")

    def test_request_and_escalation_are_independent_facts(self):
        for live in (False, True):
            row, calls, measured = self.owner(
                [{"kind": "REVIEW_REQUEST", "seq": 3}, {"kind": "ESCALATE"}],
                live=live, exit_during="status" if live else None)
            self.assertTrue(row["review_requested"])
            self.assertTrue(row["escalated"])
            self.assertEqual(row["hidden_measure"], "final_tree_after_exit")
            self.assertEqual(row["review_observation"], "after_exit")
            self.assertFalse(any(c[0] == "post" for c in calls))

    def test_missing_journal_and_session_are_unknown_not_success(self):
        row, calls, measured = self.owner(None, status=None)
        self.assertFalse(row["review_requested"])
        self.assertFalse(row["escalated"])
        self.assertIsNone(row["review_observation"])
        self.assertIsNone(row["protocol_status"])
        self.assertIsNone(row["protocol_completed"])
        self.assertEqual(row["hidden_measure"], "final_tree_after_exit")
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            self.assertEqual(drive.journal(pathlib.Path(d), "localpilot"), [])

    def test_legacy_session_record_still_reports_actual_completion(self):
        row, calls, measured = self.owner(
            [{"kind": "REVIEW_REQUEST", "seq": 3}], live=True, session_name="session.json")
        self.assertTrue(row["protocol_completed"])
        self.assertEqual(row["protocol_status"], "completed")

    def test_ambiguous_layout_is_a_failure_not_guessed_completion(self):
        with self.assertRaisesRegex(RuntimeError, "ambiguous session records"):
            self.owner(None, ambiguous=True)


class ReviewDiagnosticsTest(unittest.TestCase):
    def test_the_engine_receives_a_capture_path_only_on_explicit_opt_in(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d).resolve()
            repo = out / "t-review-good-roman-v2-1"
            for enabled in (False, True):
                with mock.patch.object(drive.subprocess, "Popen") as spawn:
                    drive.engine(args(out, cell="review-good", review_case="roman-v2",
                                      review_diagnostics=enabled), repo, ["--once"], None)
                    cmd = spawn.call_args.args[0]
                    self.assertEqual("--review-diagnostics" in cmd, enabled)
                    if enabled:
                        path = pathlib.Path(cmd[cmd.index("--review-diagnostics") + 1])
                        self.assertEqual(path, out / f"{repo.name}.review.jsonl")
                        self.assertNotEqual(path.parent, repo)
            with self.assertRaises(SystemExit):
                drive.run(args(out, review_diagnostics=True))

    def test_a_failed_review_preserves_and_identifies_its_capture(self):
        drive = load(HERE)
        def broken_review(a, repo, *other):
            (repo.parent / f"{repo.name}.review.jsonl").write_text("retained diagnostic")
            raise RuntimeError("injected review failure")
        drive.review_cell = broken_review
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            with self.assertRaises(RuntimeError):
                drive.run(args(out, cell="review-good", review_case="roman-v2", review_diagnostics=True))
            row = json.loads((out / "results.jsonl").read_text())
            self.assertEqual(row["review_case"], "roman-v2")
            self.assertTrue(row["review_diagnostics_present"])
            capture = out / row["review_diagnostics"]
            self.assertEqual(capture.read_text(), "retained diagnostic")
            self.assertTrue(row["review_fixture_hashes"])


class TurnMetadataTest(unittest.TestCase):
    def test_repaired_timeout_and_exhausted_repair_are_separate_from_final_scoring(self):
        for decision in ("AGREE", None):
            drive = load(HERE)
            def fake_engine(a, repo, extra, log):
                log.write("ENGINE role=localpilot turn_timeout_secs=600 turn_timeout_source=builtin\n"
                          "  TURN_RAILS turn_timeout_secs=43 turn_timeout_source=config\n"
                          "  TURN ended TimedOut\n"
                          "  TURN_RAILS turn_timeout_secs=43 turn_timeout_source=config\n"
                          "  TURN ended Done\n")
                log.flush()
                return subprocess.Popen([sys.executable, "-c", "pass"])
            drive.engine = fake_engine
            drive.journal = lambda *a: [{"kind": "VERDICT", "body": "AGREE round=1"}] if decision else [{"kind": "ESCALATE"}]
            with tempfile.TemporaryDirectory() as d:
                out = pathlib.Path(d) / "results"
                drive.run(args(out, cell="review-good", review_case="roman-v2"))
                row = json.loads((out / "results.jsonl").read_text())
                self.assertEqual(row["decision"], decision)
                self.assertEqual(row["no_verdict"], decision is None)
                self.assertFalse(row["killed"])
                self.assertFalse(row["review_diagnostics_present"])
                self.assertEqual(row["runtime_turn_timeouts"], 1)
                self.assertTrue(row["runtime_trace_complete"])
                self.assertEqual(row["runtime_turn_deadlines"], [{"seconds": 43, "source": "config"}] * 2)

    def test_actual_driver_kill_is_an_incomplete_trace_not_a_runtime_timeout(self):
        drive = load(HERE)
        children = []
        def fake_engine(a, repo, extra, log):
            log.write("  TURN_RAILS turn_timeout_secs=600 turn_timeout_source=builtin\n")
            log.flush()
            child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
            children.append(child)
            return child
        drive.engine = fake_engine
        drive.journal = lambda *a: []
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            drive.run(args(out, cell="review-good", wall=0.05))
            row = json.loads((out / "results.jsonl").read_text())
            self.assertTrue(row["killed"])
            self.assertEqual(row["runtime_turn_timeouts"], 0)
            self.assertFalse(row["runtime_trace_complete"])
            self.assertEqual(row["runtime_turn_stops"], [])
            self.assertIsNotNone(children[0].poll())

    def test_failed_rows_keep_observed_metadata_and_wall_cap(self):
        drive = load(HERE)
        def broken_review(a, repo, log, planted):
            log.write("  TURN_RAILS turn_timeout_secs=0 turn_timeout_source=config\n"
                      "  TURN ended TimedOut\n")
            raise RuntimeError("failed after runtime stop")
        drive.review_cell = broken_review
        with tempfile.TemporaryDirectory() as d:
            out = pathlib.Path(d) / "results"
            with self.assertRaises(RuntimeError):
                drive.run(args(out, cell="review-good", wall=900))
            row = json.loads((out / "results.jsonl").read_text())
            self.assertEqual(row["wall_cap_s"], 900)
            self.assertEqual(row["runtime_turn_timeouts"], 1)
            self.assertFalse(row["runtime_trace_complete"])
            self.assertEqual(row["runtime_turn_deadlines"], [{"seconds": 0, "source": "config"}])

    def test_legacy_or_missing_metadata_is_unknown_not_an_inferred_default(self):
        drive = load(HERE)
        with tempfile.TemporaryDirectory() as d:
            path = pathlib.Path(d) / "log"
            path.write_text("ENGINE turn_timeout_secs=900 turn_timeout_source=config\n")
            metadata = drive.turn_metadata(path, {"engine_exit": 0})
            self.assertEqual(metadata["runtime_turn_deadlines"], [])
            self.assertIsNone(metadata["runtime_turn_timeouts"])
            self.assertFalse(metadata["runtime_trace_complete"])
            path.write_text("  TURN ended TimedOut\n  TURN ended Done\n")
            metadata = drive.turn_metadata(path, {"engine_exit": 0})
            self.assertEqual(metadata["runtime_turn_timeouts"], 1)
            self.assertFalse(metadata["runtime_trace_complete"])


if __name__ == "__main__":
    unittest.main()
