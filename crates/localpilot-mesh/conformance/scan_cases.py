#!/usr/bin/env python3
"""The no-VCS scanner's boundary cases, shared by every implementation.

A tree without version control is owned through a content digest of its files.
That digest only means something if the scan records links without following
them, fails on anything it cannot read, and prunes exactly what `.pairignore`
says. These cases pin those rules with trees the fixture format cannot build
(links, unreadable paths), so another implementation can be compared with the
reference on the very same tree.

    python scan_cases.py list                  # the cases this platform can build
    python scan_cases.py build CASE DIR        # build CASE under DIR (DIR/tree, DIR/outside)
    python scan_cases.py expect CASE DIR       # the reference's outcome on DIR/tree, as JSON
    python scan_cases.py restore DIR           # make a built tree removable again
    python scan_cases.py check                 # the reference against every case (self-check)

`expect` prints `{"ok": true, "rows": [[path_hex, kind, size, sha256], ...], "digest": "T:..."}`,
where `path_hex` is the hex of the path's exact encoding (UTF-8, with any
undecodable byte as a surrogate escape), or `{"ok": false, "path": "<the
relative path that could not be read>"}`.
"""
from __future__ import annotations

import json, os, re, shutil, stat, subprocess, sys, tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
_VENDORED = HERE / "reference" / "pair.py"
REFERENCE = _VENDORED if _VENDORED.is_file() else HERE.parent / "scripts" / "pair.py"
sys.dont_write_bytecode = True

WINDOWS = os.name == "nt"
ROOT_USER = hasattr(os, "geteuid") and os.geteuid() == 0


def _write(base: Path, rel: str, text: str = "x\n") -> None:
    p = base.joinpath(*rel.split("/"))
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(text.encode("utf-8"))


def _link_dir(link: Path, target: Path) -> None:
    """A directory link: a junction on Windows (no privilege needed), a
    symlink elsewhere."""
    if WINDOWS:
        subprocess.run(["cmd", "/c", "mklink", "/J", str(link), str(target)], check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        os.symlink(target, link, target_is_directory=True)


def plain(tree: Path, outside: Path) -> None:
    for rel, text in {"a.txt": "alpha\n", "empty.txt": "", "src/main.rs": "fn main() {}\n",
                      "src/deep/x.txt": "deep\n", "café.txt": "accent\n"}.items():
        _write(tree, rel, text)


def mailbox_and_git_skipped(tree: Path, outside: Path) -> None:
    _write(tree, "kept.txt")
    _write(tree, ".pair-programming/active.json", "{}\n")    # the mailbox, top level only
    _write(tree, "sub/.pair-programming/kept.txt")            # not the mailbox: scanned
    _write(tree, ".git/HEAD", "ref\n")                       # .git at any depth: skipped
    _write(tree, "sub/.git/config", "x\n")


def pairignore_prune(tree: Path, outside: Path) -> None:
    _write(tree, ".pairignore", "# comment\nbuild/**\nlit/dir\nfoo/*\nout/\n")
    for rel in ("build/a", "build/sub/b", "lit/dir/x", "lit/other", "foo/a", "foo/bar/x",
                "out/o", "keep/k"):
        _write(tree, rel)


def link_to_outside_canary(tree: Path, outside: Path) -> None:
    _write(outside, "canary.txt", "never read by a scan\n")
    _write(tree, "inside.txt")
    _link_dir(tree / "link", outside)


def unreadable_file(tree: Path, outside: Path) -> None:
    _write(tree, "ok.txt")
    _write(tree, "secret/locked.txt")
    os.chmod(tree / "secret" / "locked.txt", 0)


def unlistable_dir(tree: Path, outside: Path) -> None:
    _write(tree, "ok.txt")
    _write(tree, "closed/inner.txt")
    os.chmod(tree / "closed", 0)


def newline_in_name(tree: Path, outside: Path) -> None:
    _write(tree, "a\nb.txt", "two lines in the name\n")
    _write(tree, "plain.txt")


def undecodable_names(tree: Path, outside: Path) -> None:
    """Names and a link target that are not valid UTF-8. Two such names must
    never share an encoding, or a rename between them would leave the digest
    unchanged."""
    root = os.fsencode(tree)
    # A pattern holding U+FFFD, the character a lossy decoder puts in place of
    # an undecodable byte: it must not match either name.
    _write(tree, ".pairignore", "a�.txt\n")
    for name in (b"a\xff.txt", b"a\xfe.txt"):
        with open(os.path.join(root, name), "wb") as f:
            f.write(b"bytes\n")
    os.symlink(b"target\xfd", os.path.join(root, b"link"))


CASES = {
    "plain": (plain, True),
    "mailbox-and-git-skipped": (mailbox_and_git_skipped, True),
    "pairignore-prune": (pairignore_prune, True),
    "link-to-outside-canary": (link_to_outside_canary, True),
    # POSIX permissions: not expressible on Windows, and root reads anything.
    "unreadable-file": (unreadable_file, not WINDOWS and not ROOT_USER),
    "unlistable-dir": (unlistable_dir, not WINDOWS and not ROOT_USER),
    # Windows forbids a newline in a file name.
    "newline-in-name": (newline_in_name, not WINDOWS),
    # POSIX names are bytes; Windows names are UTF-16 and cannot hold these.
    "undecodable-names": (undecodable_names, not WINDOWS),
}


def available() -> list:
    return [name for name, (_, ok) in CASES.items() if ok]


def build(name: str, base: Path) -> None:
    fn, ok = CASES[name]
    if not ok:
        raise SystemExit(f"case {name} cannot be built on this platform")
    (base / "tree").mkdir(parents=True)
    (base / "outside").mkdir(parents=True)
    fn(base / "tree", base / "outside")


def _reference():
    import importlib.util
    spec = importlib.util.spec_from_file_location("pair_reference", REFERENCE)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


ERROR_PATH = re.compile(r"^cannot (?:list|stat|read|read the link at) '(.*?)' under ", re.S)


def outcome(scan_tree, tree_digest, tree: Path) -> dict:
    """What a scanner does with `tree`: its rows and digest, or the path it
    refused on."""
    try:
        rows = scan_tree(tree)
    except SystemExit as e:
        m = ERROR_PATH.match(str(e.code))
        return {"ok": False, "path": m.group(1) if m else str(e.code)}
    return {"ok": True, "rows": [list(r) for r in rows], "digest": tree_digest(rows)}


def expect(name: str, base: Path) -> dict:
    """The reference's outcome, with each row's path as the hex of its exact
    encoding (UTF-8, undecodable bytes as surrogate escapes), since a path
    that is not valid UTF-8 has no faithful JSON string form."""
    ref = _reference()
    got = outcome(ref.scan_tree, ref.tree_digest, base / "tree")
    if got["ok"]:
        got["rows"] = [[p.encode("utf-8", "surrogatepass").hex(), k, s, d] for p, k, s, d in got["rows"]]
    return got


def restore(base: Path) -> None:
    """Undo permission changes so the directory can be removed."""
    for d, dirs, files in os.walk(base):
        for n in dirs + files:
            try:
                os.chmod(os.path.join(d, n), stat.S_IRWXU)
            except OSError:
                pass
        try:
            os.chmod(d, stat.S_IRWXU)
        except OSError:
            pass


def check(scan_tree=None, tree_digest=None) -> list:
    """Every available case against a scanner (the reference by default),
    returning the failures. The expectations are the reference's own rules,
    stated independently here, so a reference regression fails too."""
    ref = _reference()
    scan_tree = scan_tree or ref.scan_tree
    tree_digest = tree_digest or ref.tree_digest
    fails = []
    for name in available():
        base = Path(tempfile.mkdtemp(prefix="pair-scan-case-"))
        try:
            build(name, base)
            got = outcome(scan_tree, tree_digest, base / "tree")
            paths = [r[0] for r in got.get("rows", [])]
            kinds = {r[0]: r[1] for r in got.get("rows", [])}
            want_fail = {"unreadable-file": "secret/locked.txt", "unlistable-dir": "closed"}
            if name in want_fail:
                if got["ok"] or got["path"] != want_fail[name]:
                    fails.append(f"{name}: expected a refusal naming {want_fail[name]!r}, got {got}")
                continue
            if not got["ok"]:
                fails.append(f"{name}: refused {got['path']!r}")
                continue
            if name == "link-to-outside-canary":
                if kinds.get("link") != "l" or any(p.startswith("link/") for p in paths):
                    fails.append(f"{name}: the link was followed or not recorded as a link: {paths}")
            if name == "mailbox-and-git-skipped" and sorted(paths) != ["kept.txt", "sub/.pair-programming/kept.txt"]:
                fails.append(f"{name}: {paths}")
            if name == "pairignore-prune" and sorted(paths) != [".pairignore", "foo/bar/x", "keep/k", "lit/other"]:
                fails.append(f"{name}: {paths}")
            if name == "newline-in-name" and "a\nb.txt" not in paths:
                fails.append(f"{name}: {paths}")
            if name == "undecodable-names" and len(set(p for p in paths if p != ".pairignore")) != 3:
                fails.append(f"{name}: undecodable names collapsed or were ignored: {paths!r}")
        finally:
            restore(base)
            shutil.rmtree(base, ignore_errors=True)
    return fails


def main(argv=None) -> int:
    for stream in (sys.stdout, sys.stderr):
        stream.reconfigure(encoding="utf-8", errors="backslashreplace")
    args = list(sys.argv[1:] if argv is None else argv)
    if args == ["list"]:
        print("\n".join(available()))
        return 0
    if len(args) == 3 and args[0] in ("build", "expect"):
        name, base = args[1], Path(args[2])
        if name not in CASES:
            print(f"unknown case {name!r}", file=sys.stderr)
            return 2
        if args[0] == "build":
            build(name, base)
        else:
            print(json.dumps(expect(name, base), ensure_ascii=True))
        return 0
    if len(args) == 2 and args[0] == "restore":
        restore(Path(args[1]))
        return 0
    if args == ["check"]:
        fails = check()
        for f in fails:
            print("FAIL " + f)
        print(f"{'FAIL' if fails else 'OK'} {len(available())} case(s)")
        return 1 if fails else 0
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
