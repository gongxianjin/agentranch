"""Tests for the gateway-free six-field onboarding closure entry."""
from __future__ import annotations

import contextlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
ENTRY = ROOT / ".test-scripts" / "project-closure" / "run_project_closure.py"
DRIVER = ROOT / "tools" / "live_project_closure.py"


def load_entry():
    spec = importlib.util.spec_from_file_location("project_closure_onboarding_entry", ENTRY)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_driver():
    spec = importlib.util.spec_from_file_location("project_closure_onboarding_driver", DRIVER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_legacy_prepare_existing_path_is_not_exposed():
    driver = load_driver()

    assert not hasattr(driver, "prepare_existing")


def test_manager_session_is_a_context_manager_before_opening_windows(tmp_path):
    driver = load_driver()

    session = driver.manager_session(None, {}, tmp_path)

    assert hasattr(session, "__enter__")
    assert hasattr(session, "__exit__")


def test_run_help_requires_six_fields_and_never_gateway_path():
    completed = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "run", "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    for option in (
        "--host",
        "--root-user",
        "--model-base-url",
        "--model",
        "--package",
        "--package-manifest",
        "--document",
    ):
        assert option in completed.stdout
    assert "--gateway" not in completed.stdout
    assert "--key-stdin" not in completed.stdout


def test_root_run_help_requires_six_fields_and_no_gateway_template():
    root_entry = ROOT / ".test-scripts" / "project-closure" / "run_with_root.py"
    completed = subprocess.run(
        [sys.executable, "-I", str(root_entry), "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    for option in ("--host", "--root-user", "--model-base-url", "--model", "--document"):
        assert option in completed.stdout
    assert "--gateway-template" not in completed.stdout
    assert "--identity-file" not in completed.stdout


def test_action_help_exposes_six_field_and_provision_actions():
    completed = subprocess.run(
        [sys.executable, "-I", str(ENTRY), "action", "--help"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )

    assert completed.returncode == 0, completed.stderr
    for action in ("type-field", "provision", "type-document", "dispatch", "capture", "abort"):
        assert action in completed.stdout
    for field in ("host", "root-user", "root-password", "model-base-url", "model-token", "model"):
        assert field in completed.stdout
    assert "type-gateway" not in completed.stdout


def test_wait_for_admission_waits_for_async_generated_gateway(tmp_path):
    driver = load_driver()
    calls = []

    def refresh(session):
        calls.append(1)
        if len(calls) == 1:
            raise driver.ClosureCheckError("generated_gateway_missing")
        session["database"] = tmp_path / "controller.sqlite"
        session["database"].write_bytes(b"not-a-real-store")
        session["gateway"] = {"deployment": "d", "worker_id": "w", "artifact_sha256": "a"}

    session = {
        "database": None,
        "gateway": None,
        "state_root": tmp_path / "state",
        "before_outputs": set(),
        "document_sha256": "d",
        "model_token": b"token",
        "process": type("P", (), {"pid": 1, "poll": lambda self: None})(),
        "ui": type("UI", (), {"capture": lambda *args: None})(),
        "hwnd": 1,
        "evidence": tmp_path,
    }
    (tmp_path / "state").mkdir()
    original = driver._refresh_generated_gateway
    driver._refresh_generated_gateway = refresh
    try:
        facts = driver.wait_for_admission(session, timeout=0.01)
    finally:
        driver._refresh_generated_gateway = original

    assert calls
    assert facts["status"] == "NOT_ADMITTED"


def test_secret_frame_rejects_oversized_input_with_trailing_bytes():
    driver = load_driver()
    frame = driver.ONBOARDING_MAGIC + (1).to_bytes(4, "big") + b"p" \
        + (1).to_bytes(4, "big") + b"t" + b"x"

    try:
        with driver.stdin_secrets_lease(__import__("io").BytesIO(frame)):
            raise AssertionError("trailing bytes must not enter the lease")
    except driver.ClosureCheckError as exc:
        assert str(exc) == "trailing_bytes_in_secrets_frame"


def test_discover_generated_gateway_requires_this_run_output(tmp_path):
    driver = load_driver()
    state = tmp_path / "manager-state"
    provisioning = state / "provisioning"
    provisioning.mkdir(parents=True)
    old = provisioning / "worker-old"
    old.mkdir()
    (old / "gateway.json").write_text(json.dumps({"worker_id": "old"}), encoding="utf-8")
    before = {old.resolve()}

    fresh = provisioning / "worker-new"
    fresh.mkdir()
    gateway = fresh / "gateway.json"
    gateway.write_text(json.dumps({"worker_id": "new"}), encoding="utf-8")

    discovered = driver.discover_generated_gateway(state, before)

    assert discovered == gateway.resolve()


def test_prepare_onboarding_keeps_evidence_root_for_manager_session(tmp_path):
    driver = load_driver()
    package = tmp_path / "package"
    package.mkdir()
    manifest = package / "PACKAGE-MANIFEST.json"
    manifest.write_text(json.dumps({"package_dir": str(package)}), encoding="utf-8")
    document = tmp_path / "design.md"
    document.write_text("# design", encoding="utf-8")
    evidence = tmp_path / "evidence"
    evidence.mkdir()
    args = type("Args", (), {
        "host": "192.0.2.10", "root_user": "root",
        "model_base_url": "https://model.example", "model": "default",
        "package": str(package), "package_manifest": str(manifest),
        "document": str(document),
    })()

    prepared = driver.prepare_onboarding(args, (b"root-password", b"model-token"), evidence)

    assert prepared["evidence"] == evidence


def test_onboarding_secret_fields_are_decoded_for_unicode_input():
    driver = load_driver()
    session = {
        "host": "192.0.2.10",
        "root_user": "root",
        "root_password": b"root-password",
        "model_base_url": "https://model.example",
        "model_token": b"model-token",
        "model": "default",
    }

    assert driver._read_onboarding_field("root-password", session) == "root-password"
    assert driver._read_onboarding_field("model-token", session) == "model-token"


def test_refresh_generated_gateway_rejects_controller_path_escape(tmp_path):
    driver = load_driver()
    state = tmp_path / "manager-state"
    output = state / "provisioning" / "worker-new"
    output.mkdir(parents=True)
    gateway = output / "gateway.json"
    gateway.write_text(json.dumps({"controller_config": str(tmp_path / "old-controller.json")}), encoding="utf-8")
    session = {"state_root": state, "before_outputs": set(), "database": None}

    try:
        driver._refresh_generated_gateway(session)
    except driver.ClosureCheckError as exc:
        assert str(exc) == "generated_controller_path_escape"
    else:
        raise AssertionError("generated controller path must stay in this run output")


def test_refresh_generated_gateway_binds_database_without_context_manager(tmp_path):
    driver = load_driver()
    state = tmp_path / "manager-state"
    output = state / "provisioning" / "worker-new"
    output.mkdir(parents=True)
    gateway = output / "gateway.json"
    controller = output / "controller.json"
    controller_state = output / "controller-state"
    gateway.write_text(json.dumps({"controller_config": str(controller)}), encoding="utf-8")
    controller.write_text(json.dumps({"state_dir": str(controller_state)}), encoding="utf-8")
    session = {"state_root": state, "before_outputs": set(), "database": None}

    assert driver._refresh_generated_gateway(session) is True
    assert session["gateway_path"] == gateway.resolve()
    assert session["database"] == (controller_state / "controller.sqlite").resolve()


def test_discover_generated_gateway_rejects_ambiguous_new_outputs(tmp_path):
    driver = load_driver()
    state = tmp_path / "manager-state"
    provisioning = state / "provisioning"
    provisioning.mkdir(parents=True)
    before = set()
    for name in ("worker-a", "worker-b"):
        output = provisioning / name
        output.mkdir()
        (output / "gateway.json").write_text("{}", encoding="utf-8")

    try:
        driver.discover_generated_gateway(state, before)
    except driver.ClosureCheckError as exc:
        assert str(exc) == "generated_gateway_ambiguous"
    else:
        raise AssertionError("multiple fresh generated gateways must block")


def test_provisioning_failure_never_calls_dispatch(tmp_path):
    driver = load_driver()
    calls = []
    evidence = tmp_path / "evidence"

    @contextlib.contextmanager
    def secrets():
        yield b"root-password", b"model-token"

    @contextlib.contextmanager
    def manager(*_args):
        raise driver.ClosureCheckError("worker_provision_failed")
        yield  # pragma: no cover

    result = driver.run_project_closure(
        evidence,
        secrets,
        lambda _secrets, _evidence: {"mode": "onboarding"},
        manager,
        lambda _session: calls.append("dispatch"),
        lambda _session: {"status": "PASS_PROJECT_STORE_ADMISSION", "run_id": "run-1", "used_slots": 0},
    )

    assert result["status"] == "BLOCKED_OR_FAILED"
    assert result["failed_stage"] == "manager_open"
    assert calls == []
    assert "root-password" not in json.dumps(result)
    assert "model-token" not in json.dumps(result)
