"""A matching hash and legacy --help are not project/SSE readiness."""
import importlib.util
import io
import json
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[1]


def load_helper():
    spec = importlib.util.spec_from_file_location("capability_provision", ROOT / "tools/paramiko_provision.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def capability():
    return {"schema": "worker-capabilities/1", "binary": "aw-cli", "platform": "linux",
            "owner_exec_templates": ["sum-v1", "project-work-v1"],
            "model_token_frame": "u32be-utf8/1", "live_output": "AO-LIVE/1"}


class WorkerCapabilityTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()
        self.identity = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
        self.configs = {"worker": {"cgroup_root": "/sys/fs/cgroup/user.slice/user-1007.slice/user@1007.service"},
                        "gateway": {"remote_worker": "/home/operator/runtime/aw-cli",
                                    "remote_library_path": "/home/operator/runtime/lib"}}
        self.assets = {"worker": {"sha256": "a" * 64, "bytes": 10},
                       "loader": {"sha256": "b" * 64, "bytes": 20}}

    def preflight(self, tail):
        class Stream(io.BytesIO):
            def __init__(self, raw=b""):
                super().__init__(raw)
                self.channel = self
            def recv_exit_status(self):
                return 0
        class Client:
            def exec_command(self, command, timeout=30):
                return Stream(), Stream(b'{"binary":"aw-cli"}\nowner-exec --config FILE\n' + tail), Stream()
        return self.helper.remote_preflight(Client(), self.configs, self.identity, self.assets)

    def test_legacy_worker_is_rejected_despite_successful_hash_identity_and_help(self):
        with self.assertRaises(self.helper.ProvisionStageError) as caught:
            self.preflight(b"")
        self.assertEqual(caught.exception.error_type, "remote_worker_capability_unsupported")

    def test_incomplete_or_malformed_project_stream_capabilities_are_rejected(self):
        variants = [b"not-json\n", b"{}\n"]
        for field, value in (("schema", "worker-capabilities/0"), ("binary", "other"),
                             ("platform", "windows"), ("owner_exec_templates", ["sum-v1"]),
                             ("model_token_frame", None), ("live_output", None)):
            changed = dict(capability(), **{field: value})
            variants.append(json.dumps(changed).encode() + b"\n")
        for raw in variants:
            with self.subTest(response=raw):
                with self.assertRaises(self.helper.ProvisionStageError) as caught:
                    self.preflight(raw)
                self.assertEqual(caught.exception.error_type, "remote_worker_capability_unsupported")

    def test_complete_linux_project_stream_contract_is_required_for_readiness(self):
        result = self.preflight(json.dumps(capability()).encode() + b"\n")
        self.assertEqual(result["status"], "PASS_REMOTE_PREFLIGHT")
        self.assertTrue(result.get("project_work_supported"), "project capability must be checked")
        self.assertTrue(result.get("live_output_supported"), "stream protocol must be checked")


if __name__ == "__main__":
    unittest.main()
