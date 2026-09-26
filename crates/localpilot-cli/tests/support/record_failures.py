"""Run one mailbox command for the conformance runner and, when it fails, keep
what it said.

The runner discards the output of a fixture's parallel commands, so a command
that fails there leaves only its exit code behind. This wrapper keeps that
command's exit code and error text in the directory named by
LOCALPILOT_CONFORMANCE_FAILURES, one file per failing command. The test prints
them when the suite fails. A command that succeeds leaves nothing, and exit
codes and output pass through unchanged.

Two forms:
  record_failures.py --native <program> <args...>
      runs a native implementation as a child process;
  record_failures.py <args...>
      used as the runner's --reference: runs the pair.py named by
      LOCALPILOT_CONFORMANCE_REFERENCE in this process, as `python pair.py`
      would.
"""
import os
import runpy
import subprocess
import sys
import time
import traceback


def record(argv, rc, text):
    where = os.environ.get("LOCALPILOT_CONFORMANCE_FAILURES")
    if not where:
        return
    name = f"{time.time_ns()}-{os.getpid()}.txt"
    try:
        with open(os.path.join(where, name), "w", encoding="utf-8") as f:
            f.write(f"rc={rc}\nargv={argv}\n{text}\n")
    except OSError:
        pass  # diagnostics only: never change the command's own outcome


def native(argv):
    p = subprocess.run(argv, stderr=subprocess.PIPE)
    sys.stderr.buffer.write(p.stderr)
    sys.stderr.flush()
    if p.returncode:
        record(argv, p.returncode, p.stderr.decode("utf-8", errors="replace"))
    return p.returncode


def reference(args):
    pair = os.environ["LOCALPILOT_CONFORMANCE_REFERENCE"]
    argv = [pair] + args
    sys.argv = argv
    try:
        runpy.run_path(pair, run_name="__main__")
    except SystemExit as e:
        if e.code not in (None, 0):
            rc = e.code if isinstance(e.code, int) else 1
            record(argv, rc, str(e.code))
        raise
    except BaseException:
        record(argv, 1, traceback.format_exc())
        raise


if __name__ == "__main__":
    if sys.argv[1:2] == ["--native"]:
        sys.exit(native(sys.argv[2:]))
    reference(sys.argv[1:])
