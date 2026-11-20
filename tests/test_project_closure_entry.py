"""Tests for the repository-owned project-closure verification entrypoint."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / ".test-scripts" / "project-closure" / "run_project_closure.py"


def load_entry():
    spec = importlib.util.spec_from_file_location("project_closure_entry", ENTRY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_project_closure_entry_exists_and_exposes_implementation():
    entry = load_entry()

    assert entry.PROJECT_CLOSURE_IMPLEMENTATION == ROOT / "tools" / "live_project_closure.py"
    assert callable(entry.main)


def test_project_closure_entry_help_keeps_run_and_action_contract():
    completed = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    assert "run" in completed.stdout
    assert "action" in completed.stdout

    run_help = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "run", "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    assert run_help.returncode == 0, run_help.stderr
    for option in ("--evidence-dir", "--host", "--root-user", "--model-base-url", "--model", "--package", "--package-manifest", "--document", "--secrets-stdin"):
        assert option in run_help.stdout
    assert "--gateway" not in run_help.stdout
    assert "--key-stdin" not in run_help.stdout


def test_project_closure_entry_action_help_is_available():
    completed = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "action", "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    for action in ("click", "type-field", "provision", "type-document", "capture", "dispatch", "abort"):
        assert action in completed.stdout
    for field in ("host", "root-user", "root-password", "model-base-url", "model-token", "model"):
        assert field in completed.stdout
    assert "type-gateway" not in completed.stdout
