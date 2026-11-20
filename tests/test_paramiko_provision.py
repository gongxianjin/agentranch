import importlib.util
import json
import tempfile
import unittest
import hashlib
import socket
import stat
import subprocess
import sys
import threading
import types
from unittest.mock import patch
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tools" / "paramiko_provision.py"


def load_helper():
    spec = importlib.util.spec_from_file_location("paramiko_provision", HELPER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ProvisionContractTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()

    def test_frame_round_trip_keeps_secrets_out_of_metadata(self):
        metadata = {
            "schema": "worker-provision/1",
            "host": "172.28.8.221",
            "root_user": "root",
            "model_base_url": "https://172.28.8.128:18317",
            "model": "gpt-6.1-sol",
            "output_dir": str(Path(tempfile.gettempdir()) / "manager-provisioning" / "worker-test"),
        }
        frame = self.helper.encode_frame(metadata, b"root-password", b"api-key")
        decoded, root_password, token = self.helper.decode_frame(frame)
        self.assertEqual(decoded, metadata)
        self.assertEqual(root_password, b"root-password")
        self.assertEqual(token, b"api-key")
        self.assertNotIn(b"root-password", json.dumps(metadata).encode())
        self.assertNotIn(b"api-key", json.dumps(metadata).encode())

    def test_invalid_user_input_is_rejected_without_network(self):
        with self.assertRaisesRegex(ValueError, "HTTPS"):
            self.helper.validate_request({
                "schema": "worker-provision/1",
                "host": "172.28.8.221",
                "root_user": "root",
                "model_base_url": "http://model.example",
                "model": "gpt-6.1-sol",
            }, b"password", b"token")

    def test_result_redaction_never_contains_secret_bytes(self):
        result = self.helper.redacted_result(
            "BLOCKED_OR_FAILED", failed_stage="endpoint", error_type="certificate_error",
            next_step="检查第三方服务地址和证书",
            root_password=b"root-password", model_token=b"api-key",
        )
        encoded = json.dumps(result, ensure_ascii=False).encode()
        self.assertNotIn(b"root-password", encoded)
        self.assertNotIn(b"api-key", encoded)
        self.assertEqual(result["schema"], "worker-provision/1")

    def test_unique_staging_directory_never_overwrites(self):
        with tempfile.TemporaryDirectory() as temp:
            first = self.helper.create_unique_staging(Path(temp), "worker")
            second = self.helper.create_unique_staging(Path(temp), "worker")
            self.assertNotEqual(first, second)
            self.assertTrue(first.is_dir())
            self.assertTrue(second.is_dir())

    def test_endpoint_config_does_not_accept_token_field(self):
        with self.assertRaisesRegex(ValueError, "token"):
            self.helper.build_endpoint_config(
                "https://172.28.8.128:18317", "gpt-6.1-sol", "CERT", token="api-key"
            )


PROJECT_CAPABILITIES = (b'{"schema":"worker-capabilities/1","binary":"aw-cli","platform":"linux",'
                        b'"owner_exec_templates":["sum-v1","project-work-v1"],'
                        b'"model_token_frame":"u32be-utf8/1","live_output":"AO-LIVE/1"}\n')


def request_metadata():
    return {"schema": "worker-provision/1", "host": "192.0.2.10", "root_user": "root",
            "model_base_url": "https://model.example", "model": "test-model",
            "output_dir": str(Path(tempfile.gettempdir()) / "manager-provisioning" / "worker-test"),
            "attempt_id": "attempt-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}


class ProvisionBoundaryTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()

    def test_malformed_origins_and_metadata_secrets_are_rejected(self):
        for url in ("https://bad host", "https://model.example:99999", "https://model.example:abc",
                    "https://model.example\n", "https://user@model.example", "https://model.example/v1"):
            with self.subTest(url=url), self.assertRaises(ValueError):
                self.helper.validate_request(dict(request_metadata(), model_base_url=url), b"password", b"token")
        for field in ("password", "root_password", "token", "model_token", "api_key"):
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.helper.encode_frame(dict(request_metadata(), **{field: "must-not-serialize"}), b"password", b"token")

    def test_result_rejects_nested_secret_facts_and_redacts_echoes(self):
        with self.assertRaises(ValueError):
            self.helper.redacted_result("BLOCKED_OR_FAILED", facts={"api_key": "secret"})
        result = self.helper.redacted_result("BLOCKED_OR_FAILED", failed_stage="input",
            error_type="input_error", next_step="echo root-password api-key",
            root_password=b"root-password", model_token=b"api-key")
        encoded = json.dumps(result)
        self.assertNotIn("root-password", encoded)
        self.assertNotIn("api-key", encoded)

    def test_staging_prefix_cannot_escape_parent(self):
        with tempfile.TemporaryDirectory() as temp:
            for prefix in ("../outside", "/absolute", "..", "a/b", "a\\b"):
                with self.subTest(prefix=prefix), self.assertRaises(ValueError):
                    self.helper.create_unique_staging(Path(temp), prefix)

    def test_helper_requires_a_new_manager_owned_output_directory(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for output in ("relative-output", str(root), str(root / ".." / "escape")):
                with self.subTest(output=output), self.assertRaisesRegex(ValueError, "output_dir"):
                    self.helper.validate_request(
                        dict(request_metadata(), output_dir=output), b"password", b"token", require_output_dir=True,
                    )
            target = root / "provisioning" / "worker-new"
            self.helper.validate_request(
                dict(request_metadata(), output_dir=str(target)), b"password", b"token", require_output_dir=True,
            )
            self.assertFalse(target.exists(), "validation must not create the Manager-owned directory")

    def test_helper_rejects_output_directory_symlink(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            real = root / "real"
            real.mkdir()
            link = root / "link"
            try:
                link.symlink_to(real, target_is_directory=True)
            except OSError as exc:
                self.skipTest(f"directory symlink unavailable: {exc}")
            with self.assertRaisesRegex(ValueError, "symlink"):
                self.helper.validate_request(
                    dict(request_metadata(), output_dir=str(link)), b"password", b"token", require_output_dir=True,
                )

    def test_frame_rejects_truncation_trailing_and_clears_owned_secret_buffers(self):
        frame = self.helper.encode_frame(request_metadata(), b"password", b"token")
        for bad in (frame[:4], frame[:-1], frame + b"x"):
            with self.assertRaises(ValueError):
                self.helper.decode_frame(bad)
        raw = bytearray(frame)
        password = bytearray(b"password")
        token = bytearray(b"token")
        self.helper.clear_buffers(raw, password, token)
        self.assertEqual(raw, bytearray(len(frame)))
        self.assertEqual(password, bytearray(8))
        self.assertEqual(token, bytearray(5))

    def test_result_frame_keeps_private_key_outside_json(self):
        result = {"schema": "worker-provision/1", "status": "PASS_WORKER_PROVISION", "preflight": True}
        frame = self.helper.encode_result_frame(result, b"synthetic-private-key")
        magic = b"AW-WORKER-PROVISION-RESULT/1\n"
        self.assertTrue(frame.startswith(magic))
        size = int.from_bytes(frame[len(magic):len(magic) + 4], "big")
        metadata = frame[len(magic) + 4:len(magic) + 4 + size]
        self.assertNotIn(b"synthetic-private-key", metadata)
        self.assertEqual(json.loads(metadata), result)
        self.assertTrue(frame.endswith(b"synthetic-private-key"))

    def test_result_frame_round_trip_and_rejects_trailing_bytes(self):
        result = {"schema": "worker-provision/1", "status": "BLOCKED_OR_FAILED", "error_type": "preflight"}
        frame = self.helper.encode_result_frame(result, b"private-key")
        decoded, private_key = self.helper.decode_result_frame(frame)
        self.assertEqual(decoded, result)
        self.assertEqual(private_key, b"private-key")
        with self.assertRaises(ValueError):
            self.helper.decode_result_frame(frame + b"trailing")
        with self.assertRaises(ValueError):
            self.helper.decode_result_frame(frame[:-1])

    def test_asset_bundle_hashes_checked_before_use(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            worker = b"\x7fELF\x02\x01\x01" + b"\x00" * 57
            loader = worker + b"sqlite"
            (root / "aw-cli").write_bytes(worker)
            (root / "libsqlite3.so.0").write_bytes(loader)
            manifest = {"schema": "worker-provision-assets/1", "version": "0.2.0",
                "worker": {"path": "aw-cli", "bytes": len(worker), "sha256": hashlib.sha256(worker).hexdigest()},
                "loader": {"path": "libsqlite3.so.0", "bytes": len(loader), "sha256": hashlib.sha256(loader).hexdigest()}}
            path = root / "provision-assets.json"
            path.write_text(json.dumps(manifest), encoding="utf-8")
            assets = self.helper.load_assets(path, "0.2.0")
            self.assertEqual(assets["worker"]["sha256"], hashlib.sha256(worker).hexdigest())
            (root / "aw-cli").write_bytes(worker + b"changed")
            with self.assertRaises(ValueError):
                self.helper.load_assets(path, "0.2.0")
            with self.assertRaises(ValueError):
                self.helper.load_assets(path, "0.1.0")

    def test_prepare_local_provisioning_loads_explicit_trust_and_assets(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            worker = b"synthetic-worker"
            loader = b"synthetic-loader"
            (root / "aw-cli").write_bytes(worker)
            (root / "libsqlite3.so.0").write_bytes(loader)
            (root / "provision-assets.json").write_text(json.dumps({
                "schema": "worker-provision-assets/1", "version": "0.2.0",
                "worker": {"path": "aw-cli", "bytes": len(worker), "sha256": hashlib.sha256(worker).hexdigest()},
                "loader": {"path": "libsqlite3.so.0", "bytes": len(loader), "sha256": hashlib.sha256(loader).hexdigest()},
            }), encoding="utf-8")
            (root / "known_hosts").write_text("synthetic-host-key\n", encoding="ascii")
            certificate = "-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n"
            (root / "model-ca.pem").write_text(certificate, encoding="ascii")
            prepared = self.helper.prepare_local_provisioning(request_metadata(), {
                "schema": "worker-provision-prerequisites/1",
                "asset_manifest": "provision-assets.json",
                "asset_version": "0.2.0",
                "known_hosts": "known_hosts",
                "tls_certificate": "model-ca.pem",
            }, root)
            self.assertEqual(prepared["assets"]["worker"]["content"], worker)
            self.assertEqual(prepared["known_hosts"], str((root / "known_hosts").resolve()))
            self.assertEqual(prepared["endpoint"]["certificate_pem"], certificate)

    def test_prepare_local_provisioning_identifies_each_missing_prerequisite(self):
        prerequisites = {
            "schema": "worker-provision-prerequisites/1",
            "asset_manifest": "provision-assets.json",
            "asset_version": "0.2.0",
            "known_hosts": "known_hosts",
            "tls_certificate": "model-ca.pem",
        }
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            for missing, error_type in (
                ("provision-assets.json", "assets_unavailable"),
                ("known_hosts", "known_hosts_unavailable"),
                ("model-ca.pem", "tls_certificate_unavailable"),
            ):
                with self.subTest(missing=missing):
                    for path in root.iterdir():
                        path.unlink()
                    worker = b"synthetic-worker"
                    loader = b"synthetic-loader"
                    if missing != "provision-assets.json":
                        (root / "aw-cli").write_bytes(worker)
                        (root / "libsqlite3.so.0").write_bytes(loader)
                        (root / "provision-assets.json").write_text(json.dumps({
                            "schema": "worker-provision-assets/1", "version": "0.2.0",
                            "worker": {"path": "aw-cli", "bytes": len(worker), "sha256": hashlib.sha256(worker).hexdigest()},
                            "loader": {"path": "libsqlite3.so.0", "bytes": len(loader), "sha256": hashlib.sha256(loader).hexdigest()},
                        }), encoding="utf-8")
                    if missing != "known_hosts":
                        (root / "known_hosts").write_text("synthetic-host-key\n", encoding="ascii")
                    if missing != "model-ca.pem":
                        (root / "model-ca.pem").write_text(
                            "-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n", encoding="ascii")
                    with self.assertRaises(self.helper.LocalPrerequisiteError) as caught:
                        self.helper.prepare_local_provisioning(request_metadata(), prerequisites, root)
                    self.assertEqual(caught.exception.error_type, error_type)

    def test_unknown_host_is_rejected_before_password_authentication(self):
        import paramiko
        host_key = paramiko.RSAKey.generate(2048)
        authenticated = []
        class Server(paramiko.ServerInterface):
            def check_auth_password(self, username, password):
                authenticated.append(username)
                return paramiko.AUTH_SUCCESSFUL
        listener = socket.socket()
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(5)
        port = listener.getsockname()[1]
        def serve():
            transport = None
            try:
                connection, _ = listener.accept()
                transport = paramiko.Transport(connection)
                transport.add_server_key(host_key)
                transport.start_server(server=Server())
                transport.accept(2)
            except (EOFError, OSError, paramiko.SSHException):
                pass
            finally:
                if transport:
                    transport.close()
        thread = threading.Thread(target=serve, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temp:
                known = Path(temp) / "known_hosts"
                known.write_text("", encoding="ascii")
                with self.assertRaises(paramiko.SSHException):
                    self.helper.connect_ssh(
                        "127.0.0.1", "root", known,
                        password=b"never-send-this", port=port,
                    )
                self.assertEqual(known.read_text(), "")
                self.assertEqual(authenticated, [], "unknown host must fail before password auth")
        finally:
            listener.close()
            thread.join(5)

    def test_generate_worker_key_returns_private_bytes_and_public_line(self):
        private_key, public_line = self.helper.generate_worker_key()
        self.assertGreater(len(private_key), 100)
        self.assertTrue(private_key.startswith(b"-----BEGIN RSA PRIVATE KEY-----"))
        self.assertRegex(public_line, r"^ssh-rsa [A-Za-z0-9+/=]+$")
        self.assertNotIn(private_key, public_line.encode())

    def test_identity_verification_and_ordinary_user_discovery_are_bounded(self):
        class Stream:
            def __init__(self, value):
                self.value = value
                self.channel = self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return 0
            def close(self):
                pass
        class Client:
            def __init__(self, values):
                self.values = iter(values)
            def exec_command(self, _command, timeout=15):
                stdout_value = next(self.values)
                return Stream(b""), Stream(stdout_value), Stream(b"")
        self.assertTrue(self.helper.verify_root_identity(Client([b"0\n"])))
        identity = self.helper.discover_ordinary_user(Client([
            b"ubuntu:x:1000:1000::/home/ubuntu:/bin/bash\n"
        ]))
        self.assertEqual(identity["user"], "ubuntu")
        self.assertEqual(identity["home"], "/home/ubuntu")
        with self.assertRaises(ValueError):
            self.helper.discover_ordinary_user(Client([
                b"ubuntu:x:1000:1000::/home/ubuntu:/bin/bash\n"
                b"admin:x:1001:1001::/home/admin:/bin/bash\n"
            ]))

    def test_bootstrap_root_identity_verifies_root_discovers_one_user_and_closes(self):
        class Stream:
            def __init__(self, value):
                self.value = value
                self.channel = self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return 0
            def close(self):
                pass
        class Client:
            def __init__(self):
                self.outputs = iter((
                    b"0\n",
                    b"operator:x:1007:1011::/home/operator:/bin/bash\n",
                ))
                self.closed = False
            def exec_command(self, _command, timeout=15):
                return Stream(b""), Stream(next(self.outputs)), Stream(b"")
            def close(self):
                self.closed = True
        client = Client()
        calls = []
        def connector(host, user, known_hosts, *, password, port=22, timeout=15):
            calls.append((host, user, str(known_hosts), password, port, timeout))
            return client
        identity = self.helper.bootstrap_root_identity(
            request_metadata(), b"root-password", Path("known_hosts"), connector=connector
        )
        self.assertEqual(identity, {
            "user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"
        })
        self.assertEqual(calls, [("192.0.2.10", "root", "known_hosts", b"root-password", 22, 15)])
        self.assertTrue(client.closed)

    def test_bootstrap_root_identity_blocks_non_root_before_user_discovery(self):
        class Stream:
            def __init__(self, value):
                self.value = value
                self.channel = self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return 0
            def close(self):
                pass
        class Client:
            def __init__(self):
                self.closed = False
            def exec_command(self, _command, timeout=15):
                return Stream(b""), Stream(b"1000\n"), Stream(b"")
            def close(self):
                self.closed = True
        client = Client()
        with self.assertRaises(self.helper.ProvisionStageError) as caught:
            self.helper.bootstrap_root_identity(
                request_metadata(), b"root-password", Path("known_hosts"),
                connector=lambda *args, **kwargs: client,
            )
        self.assertEqual(caught.exception.error_type, "root_identity_invalid")
        self.assertTrue(client.closed)

    def test_bootstrap_ordinary_access_installs_key_and_verifies_key_login(self):
        class Stream:
            def __init__(self, value):
                self.value = value
                self.channel = self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return 0
            def close(self):
                pass
        class Sftp:
            def __init__(self):
                self.content = b"ssh-rsa existing-key\n"
            def lstat(self, path):
                if path == "/home":
                    return types.SimpleNamespace(st_mode=stat.S_IFDIR | 0o755, st_uid=0, st_gid=0)
                if path.endswith("authorized_keys"):
                    return types.SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=1007, st_gid=1011)
                return types.SimpleNamespace(st_mode=stat.S_IFDIR | 0o700, st_uid=1007, st_gid=1011)
            def open(self, path, mode):
                class File:
                    def __init__(self, outer, append):
                        self.outer, self.append = outer, append
                    def read(self, _limit=-1):
                        return self.outer.content[:_limit]
                    def write(self, raw):
                        if not self.append:
                            raise AssertionError("authorization must be append-only")
                        self.outer.content += raw
                    def flush(self):
                        pass
                    def __enter__(self):
                        return self
                    def __exit__(self, *_args):
                        return False
                return File(self, mode == "ab")
            def close(self):
                pass
        class RootClient:
            def __init__(self, sftp):
                self.sftp = sftp
                self.closed = False
            def exec_command(self, command, timeout=15):
                values = {
                    "id -u": b"0\n",
                    "getent passwd": b"operator:x:1007:1011::/home/operator:/bin/bash\n",
                }
                return Stream(b""), Stream(values[command]), Stream(b"")
            def open_sftp(self):
                return self.sftp
            def close(self):
                self.closed = True
        class OrdinaryClient:
            def __init__(self):
                self.closed = False
            def exec_command(self, command, timeout=15):
                class Stream:
                    def __init__(self, value):
                        self.value = value
                        self.channel = self
                    def read(self, _limit=-1):
                        return self.value
                    def recv_exit_status(self):
                        return 0
                    def close(self):
                        pass
                values = {"id -u": b"1007\n", "id -un": b"operator\n"}
                return Stream(b""), Stream(values[command]), Stream(b"")
            def close(self):
                self.closed = True
        sftp = Sftp()
        root_client = RootClient(sftp)
        ordinary_client = OrdinaryClient()
        calls = []
        def connector(host, user, known_hosts, *, password, port=22, timeout=15):
            calls.append((host, user, password, port, timeout))
            return root_client
        def key_connector(host, user, known_hosts, *, private_key, port=22, timeout=15):
            calls.append((host, user, private_key, port, timeout))
            return ordinary_client
        private_key = b"synthetic-private-key"
        public_line = "ssh-rsa synthetic-public-key"
        result = self.helper.bootstrap_ordinary_access(
            request_metadata(), b"root-password", Path("known_hosts"),
            identity={"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"},
            key_generator=lambda: (private_key, public_line),
            connector=connector,
            key_connector=key_connector,
        )
        self.assertEqual(result["identity"]["user"], "operator")
        self.assertEqual(result["private_key"], private_key)
        self.assertEqual(result["public_key"], public_line)
        self.assertIn(public_line.encode(), sftp.content.splitlines())
        self.assertTrue(root_client.closed)
        self.assertTrue(ordinary_client.closed)
        self.assertEqual(calls[0][1], "root")
        self.assertEqual(calls[1][1], "operator")

    def test_bootstrap_ordinary_access_rejects_identity_mismatch_without_key_login(self):
        class Client:
            def __init__(self):
                self.closed = False
            def exec_command(self, command, timeout=15):
                class Stream:
                    def __init__(self, value):
                        self.value = value
                        self.channel = self
                    def read(self, _limit=-1):
                        return self.value
                    def recv_exit_status(self):
                        return 0
                    def close(self):
                        pass
                return Stream(b""), Stream(b"1000\n"), Stream(b"")
            def close(self):
                self.closed = True
        root_client = Client()
        with self.assertRaises(self.helper.ProvisionStageError) as caught:
            self.helper.bootstrap_ordinary_access(
                request_metadata(), b"root-password", Path("known_hosts"),
                identity={"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"},
                connector=lambda *args, **kwargs: root_client,
                key_connector=lambda *args, **kwargs: self.fail("ordinary login must not run"),
            )
        self.assertEqual(caught.exception.error_type, "root_identity_invalid")
        self.assertTrue(root_client.closed)

    def test_stage_local_provisioning_creates_fresh_configs_without_tokens(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            endpoint = {
                "schema": "model-endpoint/1",
                "base_url": "https://model.example",
                "model": "test-model",
                "certificate_pem": "-----BEGIN CERTIFICATE-----x-----END CERTIFICATE-----",
                "timeout_secs": 240,
            }
            identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
            assets = {
                "worker": {"path": "/bundle/aw-cli", "bytes": 100, "sha256": "a" * 64, "content": b"worker"},
                "loader": {"path": "/bundle/libsqlite3.so.0", "bytes": 200, "sha256": "b" * 64, "content": b"loader"},
            }
            output_dir = root / "linux221-a1b2c3d4e5f6"
            staged = self.helper.stage_local_provisioning(
                output_dir, "linux221-a1b2c3d4e5f6", "worker-b", "192.0.2.10", 22,
                identity=identity, known_hosts=root / "known_hosts",
                private_key=b"synthetic-private-key", protocol_key=b"p" * 32,
                remote_root="/home/operator/cross-runtime-a1b2c3d4e5f6",
                assets=assets, endpoint=endpoint,
            )
            output = Path(staged["output_dir"])
            self.assertTrue(output.is_dir())
            self.assertFalse((output / "ordinary-user-rsa").exists(), "SSH private key stays in the result pipe, not on disk")
            self.assertEqual((output / "protocol.key").read_bytes(), b"70" * 32 + b"\n")
            self.assertEqual(bytes.fromhex((output / "protocol.key").read_text().strip()), b"p" * 32)
            self.assertFalse(any(b"synthetic-private-key" in path.read_bytes() for path in output.iterdir() if path.is_file()))
            self.assertEqual(json.loads((output / "gateway.json").read_text()), staged["configs"]["gateway"])
            self.assertEqual(json.loads((output / "model-endpoint.json").read_text()), endpoint)
            serialized = json.dumps(staged, ensure_ascii=False)
            self.assertNotIn("token", serialized.lower())
            self.assertNotIn("api-secret", serialized)
            with self.assertRaises(FileExistsError):
                self.helper.stage_local_provisioning(
                    output, "linux221-a1b2c3d4e5f6", "worker-b", "192.0.2.10", 22,
                    identity=identity, known_hosts=root / "known_hosts",
                    private_key=b"synthetic-private-key", protocol_key=b"p" * 32,
                    remote_root="/home/operator/cross-runtime-a1b2c3d4e5f6",
                    assets=assets, endpoint=endpoint,
                )

    def test_preflight_uses_the_verified_loader_for_all_cli_probes(self):
        import io
        import re
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        assets = {"worker": {"sha256": "a" * 64, "bytes": 10}, "loader": {"sha256": "b" * 64, "bytes": 20}}
        configs = {"worker": {"cgroup_root": "/sys/fs/cgroup/user.slice/user-1007.slice/user@1007.service"},
                   "gateway": {"remote_worker": "/home/operator/cross-runtime-test/aw-cli", "remote_library_path": "/home/operator/cross-runtime-test/lib"}}
        class Stream(io.BytesIO):
            def __init__(self, raw=b""):
                super().__init__(raw)
                self.channel = self
            def recv_exit_status(self):
                return 0
        class Client:
            def exec_command(self, command, timeout=30):
                # Model a host whose SQLite is available only from the bundled loader.
                probes = re.findall(r"(?:[^;]+) --(?:identity|help|capabilities)", command)
                if len(probes) != 3 or any("LD_LIBRARY_PATH=/home/operator/cross-runtime-test/lib" not in probe for probe in probes):
                    return Stream(), Stream(), Stream(b"error while loading shared libraries: libsqlite3.so.0")
                return Stream(), Stream(b'{"binary":"aw-cli"}\nowner-exec --config FILE\n' + PROJECT_CAPABILITIES), Stream()
        self.assertEqual(self.helper.remote_preflight(Client(), configs, identity, assets)["status"], "PASS_REMOTE_PREFLIGHT")

    def test_remote_preflight_checks_owner_artifacts_cgroup_and_cli_identity(self):
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        assets = {
            "worker": {"path": "/bundle/aw-cli", "bytes": 100, "sha256": "a" * 64, "content": b"worker"},
            "loader": {"path": "/bundle/libsqlite3.so.0", "bytes": 200, "sha256": "b" * 64, "content": b"loader"},
        }
        endpoint = {
            "schema": "model-endpoint/1", "base_url": "https://model.example",
            "model": "test-model", "certificate_pem": "-----BEGIN CERTIFICATE-----x-----END CERTIFICATE-----",
            "timeout_secs": 240,
        }
        configs = self.helper.build_internal_configs(
            output_dir="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6",
            deployment="linux221-a1b2c3d4e5f6", worker_id="worker-b", host="192.0.2.10", port=22,
            identity=identity, known_hosts="C:/manager-state/known_hosts",
            identity_file="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6/ordinary-user-rsa",
            protocol_key_file="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6/protocol.key",
            remote_root="/home/operator/cross-runtime-a1b2c3d4e5f6", assets=assets, endpoint=endpoint,
        )
        class Stream:
            def __init__(self, value, status=0):
                self.value, self.status, self.channel = value, status, self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return self.status
            def close(self):
                pass
        class Client:
            def __init__(self, status=0):
                self.status = status
                self.command = None
            def exec_command(self, command, timeout=30):
                self.command = command
                return Stream(b"", self.status), Stream(b'{"binary":"aw-cli"}\nusage: owner-exec --config FILE\n' + PROJECT_CAPABILITIES, self.status), Stream(b"", self.status)
        client = Client()
        result = self.helper.remote_preflight(client, configs, identity, assets)
        self.assertEqual(result["status"], "PASS_REMOTE_PREFLIGHT")
        self.assertTrue(result["owner_exec_supported"])
        self.assertIn("id -u", client.command)
        self.assertIn("sha256sum", client.command)
        self.assertIn("owner-exec --config FILE", result["stdout"])
        self.assertNotIn("api-secret", client.command)
        with self.assertRaises(self.helper.ProvisionStageError) as caught:
            self.helper.remote_preflight(Client(status=1), configs, identity, assets)
        self.assertEqual(caught.exception.error_type, "remote_preflight_failed")

    def test_upload_staged_worker_creates_new_remote_tree_and_verifies_hashes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            files = {
                "aw-cli": b"worker-binary",
                "libsqlite3.so.0": b"loader-binary",
                "worker.json": b'{"schema":"native-worker-config/1"}',
                "model-endpoint.json": b'{"schema":"model-endpoint/1"}',
                "protocol.key": b"p" * 32,
            }
            local = {}
            for name, content in files.items():
                path = root / name
                path.write_bytes(content)
                local[name] = path
            identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
            remote_root = "/home/operator/cross-runtime-a1b2c3d4e5f6"
            class Sftp:
                def __init__(self):
                    self.remote = {}
                    self.modes = {}
                def put(self, local_path, remote_path):
                    self.remote[remote_path] = Path(local_path).read_bytes()
                def chmod(self, remote_path, mode):
                    self.modes[remote_path] = mode
                def close(self):
                    pass
            class Client:
                def __init__(self):
                    self.sftp = Sftp()
                    self.commands = []
                def exec_command(self, command, timeout=30):
                    self.commands.append(command)
                    class Stream:
                        def __init__(self, value=b"", status=0):
                            self.value, self.status, self.channel = value, status, self
                        def read(self, _limit=-1):
                            return self.value
                        def recv_exit_status(self):
                            return self.status
                        def close(self):
                            pass
                    if command.startswith("set -eu; umask 077; mkdir"):
                        return Stream(), Stream(), Stream()
                    if command.startswith("set -eu; "):
                        lines = []
                        for path, content in self.sftp.remote.items():
                            lines.extend([
                                f"{identity['uid']} {identity['gid']} {'700' if path.endswith(('aw-cli', 'libsqlite3.so.0')) else '600'} {len(content)} {path}",
                                f"{hashlib.sha256(content).hexdigest()}  {path}",
                            ])
                        return Stream(), Stream(("\n".join(lines) + "\n").encode()), Stream()
                    raise AssertionError(command)
                def open_sftp(self):
                    return self.sftp
            client = Client()
            staged = self.helper.upload_staged_worker(
                client, local, remote_root, identity,
                expected={name: {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
                          for name, content in files.items()},
            )
            self.assertEqual(staged["status"], "PASS_REMOTE_UPLOAD")
            self.assertEqual(set(client.sftp.remote.values()), set(files.values()))
            self.assertIn("mkdir", client.commands[0])
            self.assertTrue(all(mode in (0o600, 0o700) for mode in client.sftp.modes.values()))
            with self.assertRaises(self.helper.ProvisionStageError) as caught:
                broken = Client()
                broken.upload = None
                self.helper.upload_staged_worker(
                    broken, local, remote_root, identity,
                    expected={name: {"bytes": len(content), "sha256": ("0" * 64 if name == "aw-cli" else hashlib.sha256(content).hexdigest())}
                              for name, content in files.items()},
                )
            self.assertEqual(caught.exception.error_type, "remote_upload_failed")

    def test_provision_worker_uploads_consumer_compatible_protocol_key(self):
        # Keep staging and upload identity checks real; only SSH is synthetic.
        import io
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        class Stream(io.BytesIO):
            def __init__(self, value=b""):
                super().__init__(value)
                self.channel = self
            def recv_exit_status(self):
                return 0
        class Client:
            def __init__(self):
                self.files, self.modes = {}, {}
                self.closed = False
            def put(self, local, remote):
                self.files[remote] = Path(local).read_bytes()
            def chmod(self, remote, mode):
                self.modes[remote] = mode
            def open_sftp(self):
                return self
            def exec_command(self, command, timeout=30):
                if command.startswith("set -eu; umask 077; mkdir"):
                    output = b""
                elif command.startswith("set -eu; stat -c"):
                    lines = []
                    for path, content in self.files.items():
                        lines.extend([
                            f"1007 1011 {self.modes[path]:o} {len(content)} {path}",
                            f"{hashlib.sha256(content).hexdigest()}  {path}",
                        ])
                    output = ("\n".join(lines) + "\n").encode()
                elif command.startswith('set -eu; test "$(id -u)"'):
                    output = b'{"binary":"aw-cli"}\nusage: owner-exec --config FILE\n' + PROJECT_CAPABILITIES
                else:
                    raise AssertionError("unexpected synthetic SSH operation")
                return Stream(), Stream(output), Stream()
            def close(self):
                self.closed = True
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            contents = {"aw-cli": b"synthetic-worker", "libsqlite3.so.0": b"synthetic-loader"}
            for name, content in contents.items():
                (root / name).write_bytes(content)
            (root / "assets.json").write_text(json.dumps({
                "schema": "worker-provision-assets/1", "version": "0.2.0",
                **{kind: {"path": name, "bytes": len(contents[name]),
                          "sha256": hashlib.sha256(contents[name]).hexdigest()}
                   for kind, name in (("worker", "aw-cli"), ("loader", "libsqlite3.so.0"))},
            }), encoding="utf-8")
            (root / "known_hosts").write_text("synthetic-host-key\n", encoding="ascii")
            (root / "model.pem").write_text(
                "-----BEGIN CERTIFICATE-----\nsynthetic\n-----END CERTIFICATE-----\n", encoding="ascii")
            prerequisites = {"schema": "worker-provision-prerequisites/1", "asset_manifest": "assets.json",
                             "asset_version": "0.2.0", "known_hosts": "known_hosts", "tls_certificate": "model.pem"}
            output = root / "state" / "worker-new"
            client = Client()
            access = {"identity": identity, "private_key": b"synthetic-private-key", "public_key": "ssh-rsa public"}
            with patch.object(self.helper, "bootstrap_ordinary_access", return_value=access), \
                 patch.object(self.helper.os, "urandom", side_effect=lambda size: b"p" * size):
                result, private_key = self.helper.provision_worker(
                    dict(request_metadata(), output_dir=str(output)), b"root-password", b"api-secret",
                    prerequisites, root, output, key_connector=lambda *args, **kwargs: client,
                )
            self.assertEqual(result["status"], "PASS_WORKER_PROVISION")
            self.assertEqual(private_key, b"synthetic-private-key")
            self.assertTrue(client.closed)
            protocol = next(content for path, content in client.files.items() if path.endswith("/protocol.key"))
            self.assertEqual(protocol, b"70" * 32 + b"\n")
            self.assertEqual(bytes.fromhex(protocol.decode().strip()), b"p" * 32)
            for path in output.iterdir():
                if path.is_file():
                    for secret in (b"synthetic-private-key", b"root-password", b"api-secret"):
                        self.assertNotIn(secret, path.read_bytes())

    def test_provision_worker_composes_verified_stages_without_secret_metadata(self):
        calls = []
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        assets = {
            "worker": {"path": "/bundle/aw-cli", "bytes": 10, "sha256": "a" * 64, "content": b"worker"},
            "loader": {"path": "/bundle/libsqlite3.so.0", "bytes": 11, "sha256": "b" * 64, "content": b"loader"},
        }
        endpoint = {"schema": "model-endpoint/1", "base_url": "https://model.example", "model": "test-model",
                    "certificate_pem": "-----BEGIN CERTIFICATE-----x-----END CERTIFICATE-----", "timeout_secs": 240}
        class Client:
            def close(self):
                calls.append("close")
        stage_root = Path(tempfile.mkdtemp())
        (stage_root / "worker.json").write_bytes(b"worker-config")
        (stage_root / "model-endpoint.json").write_bytes(b"endpoint-config")
        with patch.object(self.helper, "prepare_local_provisioning", return_value={"assets": assets, "known_hosts": "known_hosts", "endpoint": endpoint}) as prepare, \
             patch.object(self.helper, "bootstrap_ordinary_access", return_value={"identity": identity, "private_key": b"private", "public_key": "ssh-rsa public"}) as bootstrap, \
             patch.object(self.helper, "build_internal_configs", side_effect=AssertionError("configs must be built by staging")) as build_configs, \
             patch.object(self.helper, "stage_local_provisioning", return_value={"output_dir": str(stage_root), "configs": {"gateway": {"worker_id": "worker-b"}}}) as stage, \
             patch.object(self.helper, "connect_ordinary_ssh", return_value=Client()) as connect, \
             patch.object(self.helper, "upload_staged_worker", return_value={"status": "PASS_REMOTE_UPLOAD"}) as upload, \
             patch.object(self.helper, "remote_preflight", return_value={"status": "PASS_REMOTE_PREFLIGHT", "owner_exec_supported": True}) as preflight:
            metadata = dict(request_metadata(), output_dir=str(Path("C:/new").resolve()))
            result, private_key = self.helper.provision_worker(
                metadata, b"root-password", b"api-secret", {}, Path("C:/install"), Path("C:/new"),
            )
        self.assertEqual(result["status"], "PASS_WORKER_PROVISION")
        self.assertTrue(result["preflight"])
        self.assertEqual(result["gateway_path"], str(stage_root / "gateway.json"))
        self.assertEqual(private_key, b"private")
        encoded = json.dumps(result).encode()
        self.assertNotIn(b"root-password", encoded)
        self.assertNotIn(b"api-secret", encoded)
        prepare.assert_called_once()
        bootstrap.assert_called_once()
        stage.assert_called_once()
        upload.assert_called_once()
        preflight.assert_called_once()
        self.assertEqual(calls, ["close"])

    def test_build_internal_configs_uses_discovered_user_and_manager_capabilities(self):
        endpoint = {
            "schema": "model-endpoint/1",
            "base_url": "https://model.example",
            "model": "test-model",
            "certificate_pem": "-----BEGIN CERTIFICATE-----x-----END CERTIFICATE-----",
            "timeout_secs": 240,
        }
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        assets = {
            "worker": {"path": "/bundle/aw-cli", "bytes": 100, "sha256": "a" * 64, "content": b"worker"},
            "loader": {"path": "/bundle/libsqlite3.so.0", "bytes": 200, "sha256": "b" * 64, "content": b"loader"},
        }
        configs = self.helper.build_internal_configs(
            output_dir="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6",
            deployment="linux221-a1b2c3d4e5f6",
            worker_id="worker-b",
            host="192.0.2.10",
            port=22,
            identity=identity,
            known_hosts="C:/manager-state/known_hosts",
            identity_file="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6/ordinary-user-rsa",
            protocol_key_file="C:/manager-state/provisioned/linux221-a1b2c3d4e5f6/protocol.key",
            remote_root="/home/operator/cross-runtime-a1b2c3d4e5f6",
            assets=assets,
            endpoint=endpoint,
        )
        controller = configs["controller"]
        gateway = configs["gateway"]
        worker = configs["worker"]
        assert controller["nodes"][0]["templates"] == ["sum-v1", "control-v1", "project-work-v1"]
        assert controller["nodes"][0]["address"] == "192.0.2.10:22"
        assert gateway["user"] == "operator"
        assert gateway["remote_worker"] == "/home/operator/cross-runtime-a1b2c3d4e5f6/aw-cli"
        assert worker["worker_id"] == "worker-b"
        assert worker["artifact_sha256"] == "a" * 64
        assert "token" not in json.dumps(configs).lower()

    def test_public_key_install_creates_missing_paths_with_safe_owner_and_modes(self):
        import errno
        import io
        identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        class Sftp:
            def __init__(self):
                self.attrs = {"/home": types.SimpleNamespace(st_mode=stat.S_IFDIR | 0o755, st_uid=0, st_gid=0),
                              "/home/operator": types.SimpleNamespace(st_mode=stat.S_IFDIR | 0o755, st_uid=1007, st_gid=1011)}
                self.content = b""
            def lstat(self, path):
                if path not in self.attrs:
                    raise FileNotFoundError(errno.ENOENT, "synthetic missing path")
                return self.attrs[path]
            def mkdir(self, path, mode):
                if path in self.attrs:
                    raise FileExistsError(path)
                self.attrs[path] = types.SimpleNamespace(st_mode=stat.S_IFDIR | mode, st_uid=0, st_gid=0)
            def chown(self, path, uid, gid):
                self.attrs[path].st_uid, self.attrs[path].st_gid = uid, gid
            def chmod(self, path, mode):
                self.attrs[path].st_mode = stat.S_IFMT(self.attrs[path].st_mode) | mode
            def open(self, path, mode):
                if "x" in mode:
                    if path in self.attrs:
                        raise FileExistsError(path)
                    self.attrs[path] = types.SimpleNamespace(st_mode=stat.S_IFREG | 0o600, st_uid=0, st_gid=0)
                elif path not in self.attrs:
                    raise FileNotFoundError(path)
                outer = self
                class File(io.BytesIO):
                    def close(self):
                        if "a" in mode or "x" in mode:
                            outer.content = self.getvalue()
                        super().close()
                stream = File(self.content)
                if "a" in mode:
                    stream.seek(0, 2)
                return stream
        sftp = Sftp()
        self.assertEqual(self.helper.install_public_key(sftp, "ssh-rsa synthetic-public-key", identity=identity), "APPENDED")
        self.assertEqual(sftp.content, b"ssh-rsa synthetic-public-key\n")
        self.assertEqual(self.helper.install_public_key(sftp, "ssh-rsa synthetic-public-key", identity=identity), "ALREADY_PRESENT")
        for path, mode in (("/home/operator/.ssh", 0o700), ("/home/operator/.ssh/authorized_keys", 0o600)):
            self.assertEqual(stat.S_IMODE(sftp.attrs[path].st_mode), mode)
            self.assertEqual((sftp.attrs[path].st_uid, sftp.attrs[path].st_gid), (1007, 1011))
        sftp.attrs["/home/operator/.ssh"].st_mode = stat.S_IFLNK | 0o700
        with self.assertRaises(ValueError):
            self.helper.install_public_key(sftp, "ssh-rsa another-key", identity=identity)
        self.assertEqual(sftp.content, b"ssh-rsa synthetic-public-key\n")

    def test_main_returns_success_key_only_in_binary_result(self):
        import io
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            output = root / "provisioned"
            metadata = dict(request_metadata(), output_dir=str(output))
            response = {"schema": "worker-provision/1", "status": "PASS_WORKER_PROVISION", "preflight": True,
                        "gateway_path": str(output / "gateway.json")}
            stdin = types.SimpleNamespace(buffer=io.BytesIO(self.helper.encode_frame(metadata, b"root-password", b"api-secret")))
            stdout = types.SimpleNamespace(buffer=io.BytesIO())
            with patch.object(self.helper, "__file__", str(root / "paramiko_provision.py")), \
                 patch.object(self.helper, "load_prerequisites_descriptor", return_value={}), \
                 patch.object(self.helper, "prepare_local_provisioning", return_value={}), \
                 patch.object(self.helper, "provision_worker", return_value=(response, b"synthetic-private-key")), \
                 patch.object(sys, "stdin", stdin), patch.object(sys, "stdout", stdout):
                code = self.helper.main()
            self.assertEqual(code, 0)
            result, key = self.helper.decode_result_frame(stdout.buffer.getvalue())
            self.assertEqual(result["status"], "PASS_WORKER_PROVISION")
            self.assertEqual(key, b"synthetic-private-key")
            self.assertNotIn("synthetic-private-key", json.dumps(result))
            self.assertNotIn(b"root-password", stdout.buffer.getvalue())
            self.assertNotIn(b"api-secret", stdout.buffer.getvalue())

    def test_main_preserves_safe_provision_stage_failure_without_key(self):
        import io
        metadata = request_metadata()
        stdin = types.SimpleNamespace(buffer=io.BytesIO(self.helper.encode_frame(metadata, b"root-password", b"api-secret")))
        stdout = types.SimpleNamespace(buffer=io.BytesIO())
        failure = self.helper.ProvisionStageError(
            "remote_upload_failed",
            "检查远端目录权限、文件 owner/mode 和资产 hash 后重试",
            {
                "remote_root": "/home/operator/cross-runtime-abc123",
                "remote_side_effects": "present",
                "reason_code": "remote_upload_failed",
            },
        )
        with patch.object(self.helper, "load_prerequisites_descriptor", return_value={}), \
             patch.object(self.helper, "prepare_local_provisioning", return_value={}), \
             patch.object(self.helper, "provision_worker", side_effect=failure), \
             patch.object(sys, "stdin", stdin), patch.object(sys, "stdout", stdout):
            code = self.helper.main()
        self.assertEqual(code, 2)
        result, key = self.helper.decode_result_frame(stdout.buffer.getvalue())
        self.assertEqual(result["status"], "BLOCKED_OR_FAILED")
        self.assertEqual(result["error_type"], "remote_upload_failed")
        self.assertEqual(result["attempt_id"], metadata["attempt_id"])
        self.assertEqual(result["remote_root"], "/home/operator/cross-runtime-abc123")
        self.assertEqual(result["remote_side_effects"], "present")
        self.assertEqual(result["reason_code"], "remote_upload_failed")
        self.assertEqual(key, b"")

    def test_provision_preserves_missing_local_prerequisite_before_ssh(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            output = root / "new"
            descriptor = {"schema": "worker-provision-prerequisites/1", "asset_manifest": "assets.json",
                          "asset_version": "0.2.0", "known_hosts": "known_hosts", "tls_certificate": "model.pem"}
            with self.assertRaises(self.helper.LocalPrerequisiteError) as caught:
                self.helper.provision_worker(
                    dict(request_metadata(), output_dir=str(output)), b"root-password", b"api-secret",
                    descriptor, root, output, connector=lambda *a, **k: self.fail("must not connect"))
            self.assertEqual(caught.exception.error_type, "assets_unavailable")
            self.assertFalse(output.exists())

    def test_helper_process_returns_versioned_result_frame_on_block(self):
        frame = self.helper.encode_frame(request_metadata(), b"password", b"token")
        process = subprocess.run(
            [sys.executable, "-I", "-B", str(HELPER)],
            input=frame,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(process.returncode, 2)
        metadata, private_key = self.helper.decode_result_frame(process.stdout)
        self.assertEqual(metadata["schema"], "worker-provision/1")
        self.assertEqual(metadata["status"], "BLOCKED_OR_FAILED")
        self.assertEqual(metadata["error_type"], "prerequisites_descriptor_unavailable")
        self.assertEqual(metadata["attempt_id"], request_metadata()["attempt_id"])
        self.assertTrue(metadata["next_step"])
        self.assertEqual(private_key, b"")
        self.assertNotIn(b"password", process.stdout)
        self.assertNotIn(b"token", process.stdout)

    def test_helper_without_manager_output_dir_blocks_instead_of_passing(self):
        metadata = request_metadata()
        metadata.pop("output_dir")
        process = subprocess.run(
            [sys.executable, "-I", "-B", str(HELPER)],
            input=self.helper.encode_frame(metadata, b"password", b"token"),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(process.returncode, 2)
        result, private_key = self.helper.decode_result_frame(process.stdout)
        self.assertEqual(result["status"], "BLOCKED_OR_FAILED")
        self.assertNotEqual(result.get("status"), "PASS_WORKER_PROVISION")
        self.assertEqual(private_key, b"")

    def test_identity_verification_rejects_stderr_or_nonzero_status(self):
        class Stream:
            def __init__(self, value, status):
                self.value, self.status, self.channel = value, status, self
            def read(self, _limit=-1):
                return self.value
            def recv_exit_status(self):
                return self.status
            def close(self):
                pass
        class Client:
            def exec_command(self, _command, timeout=15):
                return Stream(b"", 0), Stream(b"0\n", 1), Stream(b"denied\\n", 0)
        with self.assertRaises(ValueError):
            self.helper.verify_root_identity(Client())

    def test_public_key_install_uses_discovered_home_and_owner(self):
        class Stream:
            def __init__(self, store, append):
                self.store, self.append = store, append
            def read(self, limit=-1):
                return bytes(self.store[:limit])
            def write(self, raw):
                if not self.append:
                    raise AssertionError("authorization must be append-only")
                self.store.extend(raw)
            def flush(self):
                pass
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
        class Sftp:
            def __init__(self):
                self.store = bytearray(b"ssh-rsa prior-key\n")
            def lstat(self, path):
                modes = {"/home": (stat.S_IFDIR | 0o755, 0, 0),
                         "/home/operator": (stat.S_IFDIR | 0o750, 1007, 1011),
                         "/home/operator/.ssh": (stat.S_IFDIR | 0o700, 1007, 1011),
                         "/home/operator/.ssh/authorized_keys": (stat.S_IFREG | 0o600, 1007, 1011)}
                mode, uid, gid = modes[path]
                return types.SimpleNamespace(st_mode=mode, st_uid=uid, st_gid=gid)
            def open(self, path, mode):
                if path != "/home/operator/.ssh/authorized_keys":
                    raise AssertionError("wrong user's authorization file")
                return Stream(self.store, mode == "ab")
        sftp = Sftp()
        identity = {"user": "operator", "home": "/home/operator", "uid": 1007, "gid": 1011}
        self.assertEqual(self.helper.install_public_key(sftp, "ssh-rsa new-key", identity=identity), "APPENDED")
        self.assertEqual(bytes(sftp.store), b"ssh-rsa prior-key\nssh-rsa new-key\n")
        self.assertEqual(self.helper.install_public_key(sftp, "ssh-rsa new-key", identity=identity), "ALREADY_PRESENT")
        for bad in (dict(identity, home="/root"), dict(identity, uid=0),
                    dict(identity, gid=65534), dict(identity, user="../operator")):
            with self.subTest(identity=bad), self.assertRaises(ValueError):
                self.helper.install_public_key(sftp, "ssh-rsa new-key", identity=bad)

    def test_install_public_key_is_append_only_and_rejects_unsafe_paths(self):
        class Attrs:
            st_mode = stat.S_IFDIR | 0o700
            st_uid = 1000
            st_gid = 1000
        class Stream:
            def __init__(self, store, mode):
                self.store, self.mode = store, mode
            def read(self, _limit=-1):
                return bytes(self.store)
            def write(self, value):
                self.store.extend(value)
            def flush(self):
                pass
            def __enter__(self):
                return self
            def __exit__(self, *_args):
                return False
        class Sftp:
            def __init__(self):
                self.store = bytearray(b"ssh-rsa existing\n")
            def lstat(self, path):
                attrs = Attrs()
                if path == "/home":
                    attrs.st_uid = attrs.st_gid = 0
                if path.endswith("authorized_keys"):
                    attrs.st_mode = stat.S_IFREG | 0o600
                return attrs
            def open(self, path, mode):
                return Stream(self.store, mode)
        sftp = Sftp()
        result = self.helper.install_public_key(sftp, "ssh-rsa new-key")
        self.assertEqual(result, "APPENDED")
        self.assertEqual(self.helper.install_public_key(sftp, "ssh-rsa new-key"), "ALREADY_PRESENT")
        self.assertEqual(bytes(sftp.store).count(b"ssh-rsa new-key"), 1)
        with self.assertRaises(ValueError):
            self.helper.install_public_key(sftp, "ssh-ed25519 new-key")


if __name__ == "__main__":
    unittest.main()
