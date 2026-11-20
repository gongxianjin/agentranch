"""Tests for the one-command root-password project closure runner."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / ".test-scripts" / "project-closure" / "run_with_root.py"


def load_entry():
    spec = importlib.util.spec_from_file_location("project_closure_root_run", ENTRY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_root_run_help_exposes_only_root_and_closure_inputs():
    completed = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    for option in ("--host", "--root-user", "--model-base-url", "--model", "--evidence-dir", "--package", "--package-manifest", "--document"):
        assert option in completed.stdout
    assert "root password" in completed.stdout.lower()
    assert "model api key" in completed.stdout.lower()
    assert "--gateway-template" not in completed.stdout
    assert "--identity-file" not in completed.stdout
    assert "--key-stdin" not in completed.stdout


def test_root_run_entry_exposes_callable_main():
    entry = load_entry()
    assert callable(entry.main)
