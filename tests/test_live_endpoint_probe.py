"""Behavior checks for the secret-free live transport prerequisite probe."""
from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import socket
import ssl
import subprocess
import sys
import threading

import paramiko
import pytest
from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID
from datetime import datetime, timedelta, timezone

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts/project-closure/probe_live_endpoints.py"


def load_probe():
    spec = importlib.util.spec_from_file_location("live_endpoint_probe", SCRIPT)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


@pytest.mark.parametrize("url", [
    "http://model.example", "https://user:password@model.example",
    "https://model.example/v1", "https://model.example?token=value",
    "https://model.example#fragment", "https://model.example:99999",
])
def test_rejects_non_origin_or_credential_bearing_url(url):
    with pytest.raises(ValueError):
        load_probe().parse_origin(url)


def test_parses_dns_name_and_retains_explicit_port():
    assert load_probe().parse_origin("https://boapi.uno:18317") == ("boapi.uno", 18317)


def tls_server(tmp_path, response):
    key = rsa.generate_private_key(public_exponent=65537, key_size=2048)
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "localhost")])
    now = datetime.now(timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name)
            .public_key(key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now - timedelta(minutes=1)).not_valid_after(now + timedelta(days=1))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName("localhost")]), critical=False)
            .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
            .sign(key, hashes.SHA256()))
    cert_path, key_path = tmp_path / "cert.pem", tmp_path / "key.pem"
    cert_path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))
    key_path.write_bytes(key.private_bytes(serialization.Encoding.PEM,
                                         serialization.PrivateFormat.PKCS8,
                                         serialization.NoEncryption()))
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert_path, key_path)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(5)
    port = listener.getsockname()[1]
    requests = []

    def serve():
        try:
            raw, _ = listener.accept()
            with raw:
                try:
                    with context.wrap_socket(raw, server_side=True) as connection:
                        requests.append(connection.recv(8192))
                        connection.sendall(response)
                except (ConnectionResetError, OSError, ssl.SSLError):
                    pass  # expected for the negative hostname-verification case
        finally:
            listener.close()

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    return port, cert_path, requests, thread


def test_authenticated_tls_can_report_api_auth_required_without_model_success(tmp_path):
    port, ca, requests, thread = tls_server(
        tmp_path, b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    result, pem = load_probe().probe_model(
        f"https://localhost:{port}", context=ssl.create_default_context(cafile=str(ca)))
    thread.join(6)
    assert result["status"] == "PASS_TLS_API_AUTH_REQUIRED"
    assert result["api_http_status"] == 401
    assert result["model_call"] == "NOT_RUN"
    assert pem.startswith("-----BEGIN CERTIFICATE-----")
    assert requests[0].startswith(b"GET /v1/models HTTP/1.1\r\n")
    assert b"Authorization:" not in requests[0]


def test_mismatched_ip_certificate_is_rejected_before_http(tmp_path):
    port, ca, requests, thread = tls_server(tmp_path, b"")
    with pytest.raises(ssl.SSLCertVerificationError):
        load_probe().probe_model(f"https://127.0.0.1:{port}",
                                context=ssl.create_default_context(cafile=str(ca)))
    thread.join(6)
    assert not requests


def test_model_api_redirect_is_not_followed_or_accepted(tmp_path):
    port, ca, requests, thread = tls_server(
        tmp_path, b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.invalid/\r\nContent-Length: 0\r\n\r\n")
    result, _ = load_probe().probe_model(
        f"https://localhost:{port}", context=ssl.create_default_context(cafile=str(ca)))
    thread.join(6)
    assert result["status"] == "BLOCKED_API_ROUTE"
    assert len(requests) == 1


def ssh_server():
    key = paramiko.RSAKey.generate(2048)
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.settimeout(5)
    port = listener.getsockname()[1]

    def serve():
        try:
            raw, _ = listener.accept()
            with raw:
                transport = paramiko.Transport(raw)
                transport.add_server_key(key)
                try:
                    transport.start_server(server=paramiko.ServerInterface())
                    transport.join(3)
                except (EOFError, OSError, paramiko.SSHException):
                    pass
                finally:
                    transport.close()
        finally:
            listener.close()

    thread = threading.Thread(target=serve, daemon=True)
    thread.start()
    return key, port, thread


def test_ssh_checks_actual_key_against_explicit_known_hosts_without_auth(tmp_path):
    key, port, thread = ssh_server()
    known = tmp_path / "known_hosts"
    known.write_text(f"[127.0.0.1]:{port} {key.get_name()} {key.get_base64()}\n", encoding="ascii")
    result = load_probe().probe_ssh("127.0.0.1", known, port=port)
    thread.join(6)
    assert result["status"] == "PASS_PINNED_SSH_HOST_KEY"
    assert result["authentication"] == "NOT_RUN"


def test_ssh_mismatch_never_updates_known_hosts(tmp_path):
    key, port, thread = ssh_server()
    other = paramiko.RSAKey.generate(2048)
    known = tmp_path / "known_hosts"
    content = f"[127.0.0.1]:{port} {other.get_name()} {other.get_base64()}\n"
    known.write_text(content, encoding="ascii")
    with pytest.raises(ValueError, match="ssh_host_key_mismatch"):
        load_probe().probe_ssh("127.0.0.1", known, port=port)
    thread.join(6)
    assert known.read_text(encoding="ascii") == content


def test_existing_evidence_is_never_overwritten(tmp_path):
    evidence = tmp_path / "evidence"
    evidence.mkdir()
    marker = evidence / "result.json"
    marker.write_text('{"old": true}', encoding="utf-8")
    result = subprocess.run([sys.executable, "-I", "-B", str(SCRIPT),
                             "--host", "192.0.2.1", "--known-hosts", str(tmp_path / "absent"),
                             "--model-base-url", "https://model.invalid",
                             "--evidence-dir", str(evidence)], capture_output=True, text=True)
    assert result.returncode == 2
    assert json.loads(marker.read_text()) == {"old": True}
