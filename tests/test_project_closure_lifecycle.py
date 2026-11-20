"""Regression tests for project-closure startup failures."""
from __future__ import annotations

import contextlib
import importlib.util
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DRIVER = ROOT / "tools" / "live_project_closure.py"


def load_driver():
    spec = importlib.util.spec_from_file_location("live_project_closure_lifecycle", DRIVER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_missing_secret_frame_writes_diagnostic_result_in_fresh_evidence(tmp_path):
    driver = load_driver()
    evidence = tmp_path / "project-closure"

    @contextlib.contextmanager
    def missing_secrets():
        raise driver.ClosureCheckError("framed_gateway_secrets_required")
        yield  # pragma: no cover

    result = driver.run_project_closure(
        evidence,
        missing_secrets,
        lambda *_args: None,
        lambda *_args: None,
        lambda *_args: None,
        lambda *_args: None,
    )

    assert result["status"] == "BLOCKED_OR_FAILED"
    assert result["failed_stage"] == "credential_acquire"
    assert result["error_type"] == "ClosureCheckError"
    assert result["failure_reason"] == "framed_gateway_secrets_required"
    assert (evidence / "result.json").is_file()


def test_remote_endpoint_missing_is_converted_to_safe_reason_code():
    driver = load_driver()

    class MissingSftp:
        def open(self, _path, _mode):
            raise FileNotFoundError("private remote path detail")

    try:
        driver.read_remote_endpoint(MissingSftp(), "/remote/model-endpoint.json")
    except driver.ClosureCheckError as exc:
        assert str(exc) == "remote_model_endpoint_missing"
    else:
        raise AssertionError("missing remote endpoint must be a safe closure reason")


def test_remote_prepare_missing_endpoint_reason_is_preserved_without_secret_text(tmp_path):
    driver = load_driver()
    evidence = tmp_path / "project-closure"

    @contextlib.contextmanager
    def secrets():
        yield b"root-password", b"token-bytes"

    def missing_endpoint(_secrets, _evidence):
        raise driver.ClosureCheckError("remote_model_endpoint_missing")

    result = driver.run_project_closure(
        evidence,
        secrets,
        missing_endpoint,
        lambda *_args: None,
        lambda *_args: None,
        lambda *_args: None,
    )

    assert result["failed_stage"] == "remote_prepare"
    assert result["error_type"] == "ClosureCheckError"
    assert result["failure_reason"] == "remote_model_endpoint_missing"
    assert "key-bytes" not in json.dumps(result)
    assert "token-bytes" not in json.dumps(result)
