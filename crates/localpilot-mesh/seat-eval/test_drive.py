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
             review_case="roman-v1")
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
            for leave in ("repo", "log", "row"):
                shutil.rmtree(out, ignore_errors=True)
                out.mkdir()
                if leave == "repo":
                    (out / name).mkdir()
                    (out / name / drive.MARKER).write_text("x")
                    (out / name / "evidence.txt").write_text("first run")
                elif leave == "log":
                    (out / f"{name}.log").write_text("first run log")
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


if __name__ == "__main__":
    unittest.main()
