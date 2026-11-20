import hashlib
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tools" / "paramiko_provision.py"


def load_helper():
    spec = importlib.util.spec_from_file_location("paramiko_provision_retry", HELPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class _Stream(io.BytesIO):
    def __init__(self, value=b"", status=0):
        super().__init__(value)
        self.channel = self
        self.status = status

    def recv_exit_status(self):
        return self.status


class _Sftp:
    def __init__(self, remote):
        self.remote = remote
        self.modes = {}
        self.put_calls = []

    def put(self, local_path, remote_path):
        self.put_calls.append(remote_path)
        self.remote[remote_path] = Path(local_path).read_bytes()

    def chmod(self, remote_path, mode):
        self.modes[remote_path] = mode

    def close(self):
        pass


class _RemoteClient:
    def __init__(self, helper, root, files, identity, version, mismatch=None):
        self.helper = helper
        self.root = root
        self.identity = identity
        self.version = version
        self.remote = {}
        self.modes = {}
        self.commands = []
        self.sftp = _Sftp(self.remote)
        self.mkdir_calls = 0
        self.mismatch = mismatch
        asset_paths = {
            "aw-cli": f"{root}/aw-cli",
            "libsqlite3.so.0": f"{root}/lib/libsqlite3.so.0",
            "worker.json": f"{root}/worker.json",
            "model-endpoint.json": f"{root}/model-endpoint.json",
            "protocol.key": f"{root}/protocol.key",
        }
        for name, content in files.items():
            path = asset_paths[name]
            self.remote[path] = content
            self.modes[path] = 0o700 if name in ("aw-cli", "libsqlite3.so.0") else 0o600
        version_path = f"{root}/.asset-version"
        self.remote[version_path] = version.encode("ascii") + b"\n"
        self.modes[version_path] = 0o600

    def open_sftp(self):
        return self.sftp

    def _command_root(self, command):
        marker = "/home/operator/cross-runtime-"
        start = command.find(marker)
        if start < 0:
            raise AssertionError(f"remote root missing from command: {command}")
        suffix = command[start + len(marker):]
        name = suffix.split("/", 1)[0].strip("'\\\"")
        return marker + name

    def _asset_paths(self, remote_root):
        return {
            "aw-cli": f"{remote_root}/aw-cli",
            "libsqlite3.so.0": f"{remote_root}/lib/libsqlite3.so.0",
            "worker.json": f"{remote_root}/worker.json",
            "model-endpoint.json": f"{remote_root}/model-endpoint.json",
            "protocol.key": f"{remote_root}/protocol.key",
        }

    def exec_command(self, command, timeout=30):
        self.commands.append(command)
        if "cat --" in command and "stat -c" in command:
            remote_root = self._command_root(command)
            lines = []
            asset_paths = self._asset_paths(remote_root)
            for name, path in asset_paths.items():
                content = self.remote[path]
                digest = hashlib.sha256(content).hexdigest()
                if name == self.mismatch and remote_root == self.root:
                    digest = "0" * 64
                mode = self.modes[path] if path in self.modes else self.sftp.modes[path]
                lines.extend([
                    f"{self.identity['uid']} {self.identity['gid']} {mode:o} {len(content)} {path}",
                    f"{digest}  {path}",
                ])
            version_path = f"{remote_root}/.asset-version"
            lines.extend([
                f"{self.identity['uid']} {self.identity['gid']} 600 {len(self.remote[version_path])} {version_path}",
                self.remote[version_path].decode("ascii").strip(),
            ])
            return _Stream(), _Stream(("\n".join(lines) + "\n").encode()), _Stream()
        if command.startswith("set -eu; umask 077; mkdir"):
            self.mkdir_calls += 1
            return _Stream(), _Stream(), _Stream()
        if "printf '%s\\n'" in command and ".asset-version" in command:
            remote_root = self._command_root(command)
            version_path = f"{remote_root}/.asset-version"
            self.remote[version_path] = self.version.encode("ascii") + b"\n"
            self.modes[version_path] = 0o600
            return _Stream(), _Stream(), _Stream()
        if command.startswith("set -eu; stat -c"):
            remote_root = self._command_root(command)
            lines = []
            asset_paths = self._asset_paths(remote_root)
            for name, path in asset_paths.items():
                content = self.remote[path]
                mode = self.modes[path] if path in self.modes else self.sftp.modes[path]
                lines.extend([
                    f"{self.identity['uid']} {self.identity['gid']} {mode:o} {len(content)} {path}",
                    f"{hashlib.sha256(content).hexdigest()}  {path}",
                ])
            return _Stream(), _Stream(("\n".join(lines) + "\n").encode()), _Stream()
        raise AssertionError(f"unexpected remote command: {command}")


class ProvisionRetryTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()

    def _local_files(self, root):
        contents = {
            "aw-cli": b"worker-binary",
            "libsqlite3.so.0": b"loader-binary",
            "worker.json": b'{"schema":"native-worker-config/1"}',
            "model-endpoint.json": b'{"schema":"model-endpoint/1"}',
            "protocol.key": b"p" * 32,
        }
        paths = {}
        for name, content in contents.items():
            path = root / name
            path.write_bytes(content)
            paths[name] = path
        return paths, contents

    def _expected(self, contents, version="0.2.0"):
        return {
            name: {
                "bytes": len(content),
                "sha256": hashlib.sha256(content).hexdigest(),
                "version": version,
            }
            for name, content in contents.items()
        }

    def test_success_result_carries_retry_attempt_identity_without_secrets(self):
        result = self.helper.redacted_result(
            "PASS_WORKER_PROVISION",
            root_password=b"root-password",
            model_token=b"api-token",
            attempt_id="attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            preflight=True,
            gateway_path="C:/state/provisioning/worker-attempt-a/gateway.json",
        )
        self.assertEqual(result["attempt_id"], "attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        self.assertNotIn("root-password", json.dumps(result))
        self.assertNotIn("api-token", json.dumps(result))
        with self.assertRaises(ValueError):
            self.helper.redacted_result(
                "PASS_WORKER_PROVISION",
                root_password=b"root-password",
                model_token=b"api-token",
                attempt_id="attempt-not-valid",
            )

    def test_retry_always_uses_new_output_identity_and_preserves_failed_output(self):
        with tempfile.TemporaryDirectory() as temp:
            state = Path(temp)
            first = self.helper.create_retry_plan(state)
            old_output = Path(first["output_dir"])
            old_output.mkdir(parents=True)
            old_gateway = old_output / "gateway.json"
            old_gateway.write_text(json.dumps({"attempt_id": first["attempt_id"], "status": "failed"}))

            second = self.helper.create_retry_plan(state)
            self.assertNotEqual(first["attempt_id"], second["attempt_id"])
            self.assertNotEqual(first["output_dir"], second["output_dir"])
            self.assertFalse(Path(second["output_dir"]).exists())
            self.assertEqual(
                old_gateway.read_text(),
                json.dumps({"attempt_id": first["attempt_id"], "status": "failed"}),
            )
            self.assertNotIn("private-key", json.dumps(second))
            self.assertNotIn("api-token", json.dumps(second))

    def test_retry_reuses_only_matching_remote_asset(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            local, contents = self._local_files(root)
            identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
            expected = self._expected(contents)
            selected = "/home/operator/cross-runtime-existing"
            client = _RemoteClient(self.helper, selected, contents, identity, "0.2.0")

            reused = self.helper.upload_staged_worker(
                client, local, "/home/operator/cross-runtime-fresh", identity,
                expected=expected, asset_version="0.2.0", reuse_remote_root=selected,
            )
            self.assertEqual(reused["remote_root"], selected)
            self.assertEqual(client.mkdir_calls, 0)
            self.assertTrue(all(fact["reused"] and not fact["uploaded"] for fact in reused["files"].values()))
            self.assertTrue(all(fact["asset_version"] == "0.2.0" for fact in reused["files"].values()))

            mismatch_client = _RemoteClient(
                self.helper, selected, contents, identity, "0.2.0", mismatch="aw-cli"
            )
            fresh = self.helper.upload_staged_worker(
                mismatch_client, local, "/home/operator/cross-runtime-fresh", identity,
                expected=expected, asset_version="0.2.0", reuse_remote_root=selected,
            )
            self.assertNotEqual(fresh["remote_root"], selected)
            self.assertTrue(all(fact["uploaded"] and not fact["reused"] for fact in fresh["files"].values()))
            self.assertGreaterEqual(mismatch_client.mkdir_calls, 1)

    def test_failed_remote_side_effect_is_recorded_without_auto_cleanup(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            local, contents = self._local_files(root)
            identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
            expected = self._expected(contents)
            client = _RemoteClient(
                self.helper, "/home/operator/cross-runtime-existing", contents,
                identity, "0.2.0", mismatch="aw-cli",
            )
            client.sftp.put = lambda *_args: (_ for _ in ()).throw(OSError("synthetic upload failure"))

            with self.assertRaises(self.helper.ProvisionStageError) as caught:
                self.helper.upload_staged_worker(
                    client, local, "/home/operator/cross-runtime-fresh", identity,
                    expected=expected, asset_version="0.2.0",
                    reuse_remote_root="/home/operator/cross-runtime-existing",
                )
            self.assertEqual(caught.exception.diagnostics["remote_side_effects"], "present")
            self.assertIn("remote_root", caught.exception.diagnostics)
            self.assertNotIn("password", json.dumps(caught.exception.diagnostics).lower())
            self.assertNotIn("token", json.dumps(caught.exception.diagnostics).lower())
            self.assertEqual(client.sftp.put_calls, [])


if __name__ == "__main__":
    unittest.main()
