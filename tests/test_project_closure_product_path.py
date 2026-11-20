"""Product-path contracts for the six-field project closure harness."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest


ROOT = Path(__file__).resolve().parents[1]
DRIVER = ROOT / "tools" / "live_project_closure.py"


def load_driver():
    spec = importlib.util.spec_from_file_location("live_project_closure_product_path", DRIVER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _generated_output(tmp_path: Path) -> tuple[Path, Path, Path]:
    state = tmp_path / "manager-state"
    output = state / "provisioning" / "worker-attempt-a"
    output.mkdir(parents=True)
    controller_state = output / "controller-state"
    gateway = output / "gateway.json"
    controller = output / "controller.json"
    gateway.write_text(
        json.dumps({"controller_config": str(controller)}), encoding="utf-8"
    )
    controller.write_text(
        json.dumps({"state_dir": str(controller_state)}), encoding="utf-8"
    )
    return state, output, gateway


def _session(driver, state: Path) -> dict:
    return {
        "state_root": state,
        "before_outputs": set(),
        "database": None,
        "gateway": None,
    }


def test_dispatch_requires_current_preflight_ledger(tmp_path):
    driver = load_driver()
    state, _output, _gateway = _generated_output(tmp_path)
    session = _session(driver, state)

    driver._refresh_generated_gateway(session)

    with pytest.raises(driver.ClosureCheckError, match="current_preflight_missing"):
        driver.require_current_preflight_admission(session)


def test_dispatch_rejects_preflight_ledger_bound_to_another_output(tmp_path):
    driver = load_driver()
    state, output, gateway = _generated_output(tmp_path)
    other = state / "provisioning" / "worker-attempt-old"
    other.mkdir(parents=True)
    ledger = {
        "schema": "worker-provision-ledger/1",
        "state": "preflight",
        "output_dir": str(other),
        "gateway_path": str(other / "gateway.json"),
        "worker_id": "worker-b",
        "host": "192.0.2.10",
        "artifact_sha256": "a" * 64,
        "artifact_bytes": 6,
    }
    (state / "provisioning-ledger.json").write_text(
        json.dumps(ledger), encoding="utf-8"
    )
    session = _session(driver, state)

    driver._refresh_generated_gateway(session)

    assert session["gateway_path"] == gateway.resolve()
    with pytest.raises(driver.ClosureCheckError, match="current_preflight_mismatch"):
        driver.require_current_preflight_admission(session)


def test_user_guide_marks_real_cross_host_and_model_closure_unverified():
    guide = (ROOT / "USER-GUIDE.md").read_text(encoding="utf-8")

    assert "本仓库的 local/fixture 回归不证明真实跨主机 onboarding、真实模型调用或生产 readiness" in guide
    assert "明确未覆盖" in guide
    assert "真实跨主机闭环仍待具备匹配包、known_hosts、TLS 与新授权材料后执行" in guide
    assert "已验证（evidence 背书，附录 A）" not in guide


def test_product_evidence_never_serializes_onboarding_secrets(tmp_path):
    driver = load_driver()
    evidence = tmp_path / "evidence"
    evidence.mkdir()
    package = tmp_path / "package"
    package.mkdir()
    manifest = package / "PACKAGE-MANIFEST.json"
    manifest.write_text(json.dumps({"package_dir": str(package)}), encoding="utf-8")
    document = tmp_path / "design.md"
    document.write_text("# design", encoding="utf-8")
    args = type(
        "Args",
        (),
        {
            "host": "192.0.2.10",
            "root_user": "root",
            "model_base_url": "https://model.example",
            "model": "default",
            "package": str(package),
            "package_manifest": str(manifest),
            "document": str(document),
        },
    )()

    driver.prepare_onboarding(
        args,
        (b"synthetic-root-password", b"synthetic-model-token"),
        evidence,
    )

    serialized = "\n".join(
        path.read_text(encoding="utf-8")
        for path in evidence.rglob("*")
        if path.is_file()
    )
    assert "synthetic-root-password" not in serialized
    assert "synthetic-model-token" not in serialized
    assert "gateway_path" not in serialized
