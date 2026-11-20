"""Offline contract tests for the pinned Linux B (.221) deployment entrypoint."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts" / "linux-deploy" / "deploy_linux_b_221.py"


def load_script():
    spec = importlib.util.spec_from_file_location("deploy_linux_b_221", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_build_worker_config_binds_fresh_deployment_and_artifact():
    deploy = load_script()
    config = deploy.build_worker_config(
        deployment="linux221-a1b2c3d4e5f6",
        artifact_sha256="a" * 64,
        timeout_secs=270,
    )
    assert config == {
        "schema": "native-worker-config/1",
        "deployment": "linux221-a1b2c3d4e5f6",
        "worker_id": "B",
        "key_file": "protocol.key",
        "cgroup_root": "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service",
        "cgroup_root_mode": "current-unit",
        "artifact_sha256": "a" * 64,
        "timeout_secs": 270,
    }


def test_build_configs_declares_sum_and_project_capabilities(tmp_path):
    deploy = load_script()
    output = tmp_path / "configs"
    protocol_key = tmp_path / "protocol.key"
    protocol_key.write_text("b" * 64 + "\n", encoding="ascii")
    (tmp_path / "ordinary-user-rsa").write_text("placeholder", encoding="ascii")
    (tmp_path / "known_hosts").write_text("placeholder", encoding="ascii")
    deploy.write_configs(
        output_dir=output,
        deployment="linux221-a1b2c3d4e5f6",
        identity_file=tmp_path / "ordinary-user-rsa",
        known_hosts=tmp_path / "known_hosts",
        local_protocol_key=protocol_key,
        remote_worker="/home/operator/cross-runtime-a1b2c3d4e5f6/aw-cli",
        remote_config="/home/operator/cross-runtime-a1b2c3d4e5f6/worker.json",
        remote_library_path="/home/operator/cross-runtime-a1b2c3d4e5f6/lib",
        artifact_sha256="a" * 64,
        artifact_bytes=60243168,
        loader_sha256="b" * 64,
        loader_bytes=1557000,
    )
    controller = json.loads((output / "controller.json").read_text(encoding="utf-8"))
    templates = controller["nodes"][0]["templates"]
    assert "sum-v1" in templates
    assert "project-work-v1" in templates


def test_build_configs_refuses_existing_output_and_writes_no_state_dirs(tmp_path):
    deploy = load_script()
    output = tmp_path / "configs"
    protocol_key = tmp_path / "protocol.key"
    protocol_key.write_text("b" * 64 + "\n", encoding="ascii")
    (tmp_path / "ordinary-user-rsa").write_text("placeholder", encoding="ascii")
    (tmp_path / "known_hosts").write_text("placeholder", encoding="ascii")
    result = deploy.write_configs(
        output_dir=output,
        deployment="linux221-a1b2c3d4e5f6",
        identity_file=tmp_path / "ordinary-user-rsa",
        known_hosts=tmp_path / "known_hosts",
        local_protocol_key=protocol_key,
        remote_worker="/home/ubuntu/cross-runtime-a1b2c3d4e5f6/aw-cli",
        remote_config="/home/ubuntu/cross-runtime-a1b2c3d4e5f6/worker.json",
        remote_library_path="/home/ubuntu/cross-runtime-a1b2c3d4e5f6/lib",
        artifact_sha256="a" * 64,
        artifact_bytes=60243168,
        loader_sha256="b" * 64,
        loader_bytes=1557000,
    )
    assert result["status"] == "PASS_FRESH_LINUX221_CONFIGS"
    gateway = json.loads((output / "gateway.json").read_text(encoding="utf-8"))
    controller = json.loads((output / "controller.json").read_text(encoding="utf-8"))
    assert gateway["deployment"] == controller["deployment"] == "linux221-a1b2c3d4e5f6"
    assert gateway["user"] == "ubuntu"
    assert gateway["worker_id"] == "B"
    assert gateway["expected_worker_timeout_secs"] == 270
    assert controller["nodes"][0]["templates"] == ["sum-v1", "project-work-v1"]
    assert not (output / "controller-state").exists()
    assert not (output / "gateway-state").exists()
    assert result["protocol_key_bytes"] == 32


def test_endpoint_payload_is_non_secret_and_schema_validated():
    deploy = load_script()
    endpoint = {
        "schema": "model-endpoint/1",
        "base_url": "https://boapi.uno:18317",
        "model": "gpt-6.1-sol",
        "certificate_pem": "-----BEGIN CERTIFICATE-----\nCERT\n-----END CERTIFICATE-----",
        "timeout_secs": 240,
        "connect_addr": "172.28.8.128:18317",
    }
    assert deploy.validate_endpoint(endpoint)["model"] == "gpt-6.1-sol"
    assert "token" not in json.dumps(deploy.validate_endpoint(endpoint)).lower()
