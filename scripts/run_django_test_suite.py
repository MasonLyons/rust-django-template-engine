"""Run Django's own template test suite against django_rust_templates.

Clones (or reuses) the requested Django branch, applies the DTL monkey-patch to
``tests/runtests.py`` so the entire suite builds templates through the Rust
engine, runs ``template_tests`` and ``template_loader``, and diffs results
against the curated known-failures file for that branch. Exits non-zero on any
unexpected failure or if the Rust engine never rendered anything.

    python scripts/run_django_test_suite.py --branch stable/6.1.x
    python scripts/run_django_test_suite.py --branch stable/5.2.x template_tests.syntax_tests

The clone defaults to ``.django-cache/<version>/`` in the repo root.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent

PATCH_NAMES = {
    "stable/5.2.x": "django_tests_use_dtl-5.2.patch",
    "stable/6.1.x": "django_tests_use_dtl-6.1.patch",
}

FAILURE_NAMES = {
    "stable/5.2.x": "known_failures-5.2.txt",
    "stable/6.1.x": "known_failures-6.1.txt",
}


def log(msg: str) -> None:
    print(f"\033[1;32m==> {msg}\033[0m", flush=True)


def ensure_clone(ref_clone: Path, branch: str) -> None:
    if ref_clone.exists() and (ref_clone / ".git").exists():
        log(f"Using existing Django clone at {ref_clone}")
        return
    log(f"Cloning Django {branch} into {ref_clone} ...")
    ref_clone.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        [
            "git", "clone", "--quiet", "--depth", "1", "--no-tags",
            "--branch", branch, "https://github.com/django/django.git",
            str(ref_clone),
        ],
        check=True,
    )


def apply_patch(ref_clone: Path, patch: Path) -> None:
    check = subprocess.run(
        ["git", "apply", "--check", str(patch)],
        cwd=ref_clone, capture_output=True, text=True,
    )
    if check.returncode != 0:
        # Either already applied, or the branch drifted from the patch.
        rev = subprocess.run(
            ["git", "apply", "-R", "--check", str(patch)],
            cwd=ref_clone, capture_output=True, text=True,
        )
        if rev.returncode == 0:
            log("Patch already applied.")
            return
        log("PATCH DOES NOT APPLY — branch drifted from expectations.")
        log(check.stderr)
        raise SystemExit(2)
    subprocess.run(["git", "apply", str(patch)], cwd=ref_clone, check=True)
    log(f"Applied {patch.name}")


def parse_test_output(output: str) -> tuple[Counter, list[str]]:
    summary: Counter = Counter()
    lines: list[str] = []
    for line in output.splitlines():
        line = line.strip()
        if line.endswith(("ok", "FAIL", "ERROR")) and "... " in line:
            ident, outcome = line.rsplit("... ", 1)
            outcome = outcome.split(" ")[0].upper()
            summary[outcome] += 1
            lines.append(f"{ident.strip()} ... {outcome.lower()}")
    return summary, sorted(lines)


def load_known_failures(path: Path) -> tuple[set[str], set[str]]:
    """(required, optional) entries. A ``?`` prefix marks an environment-
    dependent entry that may fail or pass (e.g. permission tests under root)."""
    required, optional = set(), set()
    if not path.exists():
        return required, optional
    for raw in path.read_text().splitlines():
        line = raw.split("#", 1)[0].strip()
        if line.startswith("?"):
            optional.add(line[1:].strip())
        elif line:
            required.add(line)
    return required, optional


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--branch", required=True, choices=sorted(PATCH_NAMES))
    ap.add_argument("--ref-clone", type=Path,
                    help="Django checkout to use (default: .django-cache/<version>)")
    ap.add_argument("tests", nargs="*", default=["template_tests", "template_loader"])
    args = ap.parse_args()
    if args.ref_clone is None:
        version = re.search(r"(\d+\.\d+)", args.branch).group(1)
        args.ref_clone = SCRIPT_DIR.parent / ".django-cache" / version

    patch = SCRIPT_DIR / PATCH_NAMES[args.branch]
    known_failures_path = SCRIPT_DIR / FAILURE_NAMES[args.branch]

    ensure_clone(args.ref_clone, args.branch)
    apply_patch(args.ref_clone, patch)

    required, optional = load_known_failures(known_failures_path)
    known = required | optional
    log(f"Known failures curated: {len(known)}")

    fd, count_path = tempfile.mkstemp(prefix="dtl-renders-")
    os.close(fd)
    env = {**os.environ, "PYTHONPATH": str(args.ref_clone),
           "DTL_RENDER_COUNT_FILE": count_path}
    log(f"Running {' '.join(args.tests)} ({args.branch}) ...")
    result = subprocess.run(
        [sys.executable, "runtests.py", "--parallel=1", "-v=2", *args.tests],
        cwd=args.ref_clone / "tests", env=env,
        stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    output = result.stdout
    try:
        renders = int(Path(count_path).read_text().strip() or "0")
    except FileNotFoundError:
        renders = -1
    finally:
        os.unlink(count_path)

    output = result.stdout
    if "DTL_DEBUG_OUT" in os.environ:
        Path(os.environ["DTL_DEBUG_OUT"]).write_text(output)
    summary, test_lines = parse_test_output(output)
    total = sum(summary.values())
    pct = (summary.get("OK", 0) / total * 100) if total else 0.0

    failures = [ln for ln in test_lines if not ln.endswith("ok")]
    unexpected = [ln for ln in failures if ln not in known]
    fixed = [k for k in sorted(required) if k not in failures]

    for line in failures:
        marker = "known" if line in known else "UNEXPECTED"
        print(f"  [{marker}] {line}")
    if fixed:
        print("NOTE: known failures now passing (remove the entries):")
        for k in fixed:
            print(f"  [FIXED] {k}")

    print(f"\nRenders through Rust engine: {renders}")
    print(_pct(summary))
    if renders <= 0:
        print("FAIL: the Rust engine never rendered a template — the patch did not take effect.")
        return 1
    if unexpected:
        print(f"FAIL: {len(unexpected)} unexpected failures")
        return 1
    if fixed:
        print("FAIL: prune known_failures entries for now-passing tests")
        return 1
    if result.returncode != 0 and not failures:
        print("FAIL: runtests.py exited non-zero without reporting failures")
        print(output[-4000:])
        return 1
    print(f"PASS: {args.branch} — {len(failures)} curated failures, 0 unexpected")
    return 0


def _pct(summary: Counter) -> str:
    total = sum(summary.values())
    if not total:
        return "Django test suite passing: 0.00%"
    return f"Django test suite passing: {summary.get('OK', 0) / total * 100:.2f}%"


if __name__ == "__main__":
    sys.exit(main())
