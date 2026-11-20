"""Tests for root-password-only project-closure bootstrap helpers."""
from __future__ import annotations

import importlib.util
import io
from pathlib import Path
import stat
import types


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts" / "project-closure" / "bootstrap_root.py"


def load_script():
    spec = importlib.util.spec_from_file_location("project_closure_root_bootstrap", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class RemoteFiles:
    def __init__(self, data: bytes):
        self.data = data
        self.append_count = 0

    def lstat(self, path):
        if path == "/home/ubuntu/.ssh":
            return types.SimpleNamespace(st_mode=stat.S_IFDIR | 0o700, st_uid=1000, st_gid=1000)
        if path == "/home/ubuntu/.ssh/authorized_keys":
            return types.SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=1000, st_gid=1000)
        raise AssertionError(path)

    def open(self, path, mode):
        assert path == "/home/ubuntu/.ssh/authorized_keys"
        if mode == "rb":
            return io.BytesIO(self.data)
        assert mode == "ab"
        self.append_count += 1
        owner = self

        class Append(io.BytesIO):
            def close(self):
                if not self.closed:
                    owner.data += self.getvalue()
                super().close()

        return Append()


def test_install_public_key_is_append_only_and_idempotent():
    bootstrap = load_script()
    sftp = RemoteFiles(b"ssh-ed25519 EXISTING old-key\n")
    line = "ssh-rsa NEW project-closure-key"

    assert bootstrap.install_public_key(sftp, line) == "APPENDED"
    assert sftp.data == b"ssh-ed25519 EXISTING old-key\n" + line.encode() + b"\n"
    assert bootstrap.install_public_key(sftp, line) == "ALREADY_PRESENT"
    assert sftp.append_count == 1


def test_identity_checks_require_root_and_ubuntu_contract():
    bootstrap = load_script()
    root = FakeClient(b"0\n")
    ordinary = FakeClient(b"1000\n1000\nubuntu\n/home/ubuntu\n")

    assert bootstrap.verify_root_identity(root) is True
    assert bootstrap.verify_ordinary_identity(ordinary) is True


def test_result_does_not_serialize_root_password():
    bootstrap = load_script()
    result = bootstrap.public_result("PASS_ORDINARY_USER_LOGIN", "secret-root-password")

    assert result["status"] == "PASS_ORDINARY_USER_LOGIN"
    assert "secret-root-password" not in str(result)
    assert "password" not in result


class FakeClient:
    def __init__(self, output: bytes):
        self.output = output

    def exec_command(self, command, timeout):
        stdout = io.BytesIO(self.output)
        stdout.channel = types.SimpleNamespace(recv_exit_status=lambda: 0, close=lambda: None)
        return io.BytesIO(), stdout, io.BytesIO()
