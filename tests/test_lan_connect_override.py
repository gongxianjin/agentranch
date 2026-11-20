"""LAN connect-override derivation for the six-field Worker onboarding.

Root cause (dist/live-20261120-real-01): boapi.uno resolves publicly to a
dead address while the Manager host's resolver pins it to the live LAN
proxy. The worker has no hosts entry, so its model TCP connect is refused.
The historical manual chain staged connect_addr for exactly this case.
"""
from __future__ import annotations

import ipaddress
import importlib.util
from pathlib import Path
import unittest
from urllib.parse import urlparse


ROOT = Path(__file__).resolve().parents[1]
HELPER = ROOT / "tools" / "paramiko_provision.py"


def load_helper():
    spec = importlib.util.spec_from_file_location("paramiko_provision_under_test", HELPER)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class DeriveConnectAddrTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()

    def test_private_resolution_is_staged_as_connect_addr(self):
        # Manager host resolves the name to a private LAN address -> stage it.
        derived = self.helper.derive_connect_addr("https://boapi.uno:18317", ["172.28.8.128"])
        self.assertEqual(derived, "172.28.8.128:18317")

    def test_public_resolution_is_never_overridden(self):
        for answers in (["182.139.39.10"], ["93.184.216.34", "182.139.39.10"]):
            with self.subTest(answers=answers):
                self.assertIsNone(self.helper.derive_connect_addr("https://boapi.uno:18317", answers))

    def test_ip_literal_host_is_never_overridden(self):
        self.assertIsNone(self.helper.derive_connect_addr("https://172.28.8.128:18317", ["172.28.8.128"]))

    def test_default_port_443_is_kept_explicit(self):
        derived = self.helper.derive_connect_addr("https://internal.example", ["10.1.2.3"])
        self.assertEqual(derived, "10.1.2.3:443")

    def test_no_resolvable_answer_derives_none(self):
        self.assertIsNone(self.helper.derive_connect_addr("https://boapi.uno:18317", []))


class EndpointConfigStagingTests(unittest.TestCase):
    def setUp(self):
        self.helper = load_helper()
        self.pem = "-----BEGIN CERTIFICATE-----\nAAA\n-----END CERTIFICATE-----"

    def test_build_endpoint_config_stages_connect_addr_verbatim(self):
        endpoint = self.helper.build_endpoint_config(
            "https://boapi.uno:18317", "gpt-6.1-sol", self.pem,
            connect_addr="172.28.8.128:18317")
        self.assertEqual(endpoint["connect_addr"], "172.28.8.128:18317")
        self.assertEqual(endpoint["schema"], "model-endpoint/1")
        self.assertNotIn("token", endpoint)

    def test_build_endpoint_config_omits_connect_addr_when_none(self):
        endpoint = self.helper.build_endpoint_config(
            "https://boapi.uno:18317", "gpt-6.1-sol", self.pem)
        self.assertNotIn("connect_addr", endpoint)

    def test_build_endpoint_config_rejects_invalid_connect_addr(self):
        for bad in ("172.28.8.128", "not-an-addr:18317", "999.999.999.999:18317",
                    "172.28.8.128:99999", "boapi.uno:18317"):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                self.helper.build_endpoint_config(
                    "https://boapi.uno:18317", "gpt-6.1-sol", self.pem, connect_addr=bad)

    def test_stage_local_provisioning_writes_connect_addr_into_model_endpoint(self):
        import json
        import tempfile
        endpoint = self.helper.build_endpoint_config(
            "https://boapi.uno:18317", "gpt-6.1-sol", self.pem,
            connect_addr="172.28.8.128:18317")
        with tempfile.TemporaryDirectory() as temp:
            fresh = Path(temp) / "staging"
            staged = self.helper.stage_local_provisioning(
                fresh, "worker-d0c0ffee0000", "worker-b", "192.0.2.10", 22,
                identity={"user": "ubuntu", "home": "/home/ubuntu", "uid": 1000, "gid": 1000},
                known_hosts="kh", private_key=b"pk", protocol_key=b"\x00" * 32,
                remote_root="/home/ubuntu/cross-runtime-abc123",
                assets={
                    "worker": {"path": str(Path(temp) / "w"), "bytes": 1, "sha256": "0" * 64},
                    "loader": {"path": str(Path(temp) / "l"), "bytes": 1, "sha256": "0" * 64},
                },
                endpoint=endpoint)
            written = json.loads((Path(staged["output_dir"]) / "model-endpoint.json").read_text(encoding="utf-8"))
            self.assertEqual(written.get("connect_addr"), "172.28.8.128:18317")


if __name__ == "__main__":
    unittest.main()
