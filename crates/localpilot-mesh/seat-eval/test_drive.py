"""The driver's own safety: it deletes only what it made, never runs on
changed fixtures, and never leaves an engine running. No model is needed.

    python -m unittest test_drive      (from this directory)
"""

import argparse
import importlib.util
import json
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
             wall=30, localpilot=sys.executable, provider=None, context_window=None)
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


if __name__ == "__main__":
    unittest.main()
