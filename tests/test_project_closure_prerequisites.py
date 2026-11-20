"""Tests for safe project-closure prerequisite preparation."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts" / "project-closure" / "prepare_prerequisites.py"


def load_script():
    spec = importlib.util.spec_from_file_location("project_closure_prerequisites", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _base_configs(root: Path) -> Path:
    gateway = {
        "schema": "native-ssh-gateway/1",
        "deployment": "deploy-1",
        "worker_id": "B",
        "host": "172.28.8.221",
        "port": 22,
        "user": "ubuntu",
        "known_hosts": str(root / "known_hosts"),
        "identity_file": str(root / "old-key"),
        "controller_config": str(root / "controller.json"),
        "state_dir": str(root / "old-gateway-state"),
    }
    controller = {
        "schema": "ao-reason-config/1",
        "mode": "real",
        "role": "controller",
        "deployment": "deploy-1",
        "state_dir": str(root / "old-controller-state"),
        "nodes": [{"id": "B", "address": "172.28.8.221:22", "key_file": str(root / "protocol.key")}],
    }
    gateway_path = root / "gateway.json"
    controller_path = root / "controller.json"
    gateway_path.write_text(json.dumps(gateway), encoding="utf-8")
    controller_path.write_text(json.dumps(controller), encoding="utf-8")
    return gateway_path


def test_locate_key_reports_only_matching_file_metadata(tmp_path):
    script = load_script()
    candidate = tmp_path / "ordinary-user-rsa"
    candidate.write_bytes(b"synthetic-private-key")

    found = script.locate_key_candidates([tmp_path], "ordinary-user-rsa")

    assert found == [{"path": str(candidate.resolve()), "bytes": len(b"synthetic-private-key")}]
    assert "synthetic-private-key" not in json.dumps(found)


def test_prepare_configs_uses_new_state_paths_without_creating_state_dirs(tmp_path):
    script = load_script()
    gateway_path = _base_configs(tmp_path)
    key_path = tmp_path / "ordinary-user-rsa"
    key_path.write_bytes(b"synthetic-private-key")
    output = tmp_path / "fresh-config"

    result = script.prepare_configs(gateway_path, key_path, output)

    fresh_gateway = json.loads(Path(result["gateway"]).read_text(encoding="utf-8"))
    fresh_controller = json.loads(Path(result["controller"]).read_text(encoding="utf-8"))
    assert Path(fresh_gateway["identity_file"]) == key_path.resolve()
    assert Path(fresh_gateway["controller_config"]) == Path(result["controller"]).resolve()
    assert Path(fresh_controller["state_dir"]) == (output / "controller-state").resolve()
    assert Path(fresh_gateway["state_dir"]) == (output / "gateway-state").resolve()
    assert not (output / "controller-state").exists()
    assert not (output / "gateway-state").exists()


def test_prepare_configs_refuses_existing_output_directory(tmp_path):
    script = load_script()
    gateway_path = _base_configs(tmp_path)
    key_path = tmp_path / "ordinary-user-rsa"
    key_path.write_bytes(b"synthetic-private-key")
    output = tmp_path / "fresh-config"
    output.mkdir()

    try:
        script.prepare_configs(gateway_path, key_path, output)
    except FileExistsError:
        pass
    else:
        raise AssertionError("existing output must not be overwritten")


def test_generate_key_bundle_writes_private_key_only_to_new_key_directory(tmp_path):
    script = load_script()
    key_dir = tmp_path / "key"

    record = script.generate_key_bundle(key_dir)

    private_path = Path(record["private_key_path"])
    public_path = Path(record["public_key_path"])
    assert private_path == (key_dir / "ordinary-user-rsa").resolve()
    assert public_path == (key_dir / "ordinary-user-rsa.pub").resolve()
    assert private_path.is_file()
    assert public_path.is_file()
    assert record["host"] == "172.28.8.221"
    assert record["user"] == "ubuntu"
    assert "PRIVATE KEY" not in json.dumps(record)

    import paramiko
    key = paramiko.RSAKey.from_private_key_file(str(private_path))
    assert public_path.read_text(encoding="ascii").startswith(key.get_name() + " ")
