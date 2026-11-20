#!/usr/bin/env python3
"""Bounded internal provisioning contract for the Windows Manager wizard.

The public surface is deliberately small: validate six user inputs, carry
secrets in a bounded stdin frame, and return redacted status. Network actions
are kept behind ``provision`` and must use explicit known_hosts/RejectPolicy.
This module never writes passwords or model tokens to JSON, logs, argv, or env.
"""
from __future__ import annotations

import errno
import hashlib
import ipaddress
import io
import json
import os
import re
import shlex
import stat
import struct
import time
import uuid
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit, urlparse

MAGIC = b"AW-WORKER-PROVISION/1\n"
MAX_METADATA = 64 * 1024
MAX_SECRET = 16 * 1024
MAX_FRAME = 128 * 1024
MAX_MODEL = 128
MAX_USER = 128
MAX_RESULT = 64 * 1024
MAX_PRIVATE_KEY = 64 * 1024
MAX_RESULT_FRAME = MAX_RESULT + MAX_PRIVATE_KEY + 64
FIXED_PORT = 22
PREREQUISITES_FILENAME = "provisioning-prerequisites.json"
PREREQUISITES_SCHEMA = "worker-provision-prerequisites/1"
_ALLOWED_METADATA_FIELDS = frozenset({
    "schema",
    "host",
    "root_user",
    "model_base_url",
    "model",
    "connect_addr",
    "output_dir",
    "attempt_id",
})
_SECRET_FIELD_MARKERS = (
    "password",
    "token",
    "api_key",
    "apikey",
    "secret",
    "private_key",
)


def _read_exact(raw: bytes, offset: int, length: int) -> tuple[bytes, int]:
    if length < 0 or offset + length > len(raw):
        raise ValueError("truncated worker provisioning frame")
    end = offset + length
    return raw[offset:end], end


def _validate_metadata_fields(metadata: dict[str, Any]) -> None:
    if not isinstance(metadata, dict):
        raise ValueError("metadata field must be a JSON object")
    unknown = set(metadata) - _ALLOWED_METADATA_FIELDS
    if unknown:
        raise ValueError("metadata contains unsupported fields")
    for key in metadata:
        lowered = str(key).lower()
        if any(marker in lowered for marker in _SECRET_FIELD_MARKERS):
            raise ValueError("metadata must not contain secret fields")


def _validate_origin(value: str) -> None:
    if not isinstance(value, str) or not value or any(c.isspace() or ord(c) < 32 for c in value):
        raise ValueError("model endpoint contains whitespace or control characters")
    parsed = urlsplit(value)
    if parsed.scheme != "https" or not parsed.netloc:
        raise ValueError("model endpoint must use HTTPS")
    if parsed.path not in ("", "/") or parsed.query or parsed.fragment or parsed.username or parsed.password:
        raise ValueError("model endpoint must be a bare HTTPS origin")
    try:
        hostname = parsed.hostname
        port = parsed.port
    except ValueError as exc:
        raise ValueError("model endpoint has an invalid port") from exc
    if not hostname or any(c.isspace() or ord(c) < 32 for c in hostname):
        raise ValueError("model endpoint hostname is invalid")
    if port is not None and not 1 <= port <= 65535:
        raise ValueError("model endpoint port is invalid")


def encode_frame(metadata: dict[str, Any], root_password: bytes, model_token: bytes) -> bytes:
    _validate_metadata_fields(metadata)
    if not isinstance(root_password, bytes) or not root_password or len(root_password) > MAX_SECRET:
        raise ValueError("root password field is empty or oversized")
    if not isinstance(model_token, bytes) or not model_token or len(model_token) > MAX_SECRET:
        raise ValueError("model token field is empty or oversized")
    validate_request(metadata, root_password, model_token)
    metadata_raw = json.dumps(metadata, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")
    if len(metadata_raw) > MAX_METADATA:
        raise ValueError("metadata field is oversized")
    frame = MAGIC + struct.pack(">I", len(metadata_raw)) + metadata_raw
    frame += struct.pack(">I", len(root_password)) + root_password
    frame += struct.pack(">I", len(model_token)) + model_token
    if len(frame) > MAX_FRAME:
        raise ValueError("worker provisioning frame is oversized")
    return frame


def decode_frame(raw: bytes) -> tuple[dict[str, Any], bytes, bytes]:
    if not isinstance(raw, bytes) or len(raw) > MAX_FRAME:
        raise ValueError("worker provisioning frame is oversized")
    if not raw.startswith(MAGIC):
        raise ValueError("invalid worker provisioning frame magic")
    offset = len(MAGIC)
    length_raw, offset = _read_exact(raw, offset, 4)
    metadata_length = struct.unpack(">I", length_raw)[0]
    if metadata_length > MAX_METADATA:
        raise ValueError("metadata field is oversized")
    metadata_raw, offset = _read_exact(raw, offset, metadata_length)
    password_length_raw, offset = _read_exact(raw, offset, 4)
    password_length = struct.unpack(">I", password_length_raw)[0]
    if password_length == 0 or password_length > MAX_SECRET:
        raise ValueError("root password field is empty or oversized")
    root_password, offset = _read_exact(raw, offset, password_length)
    token_length_raw, offset = _read_exact(raw, offset, 4)
    token_length = struct.unpack(">I", token_length_raw)[0]
    if token_length == 0 or token_length > MAX_SECRET:
        raise ValueError("model token field is empty or oversized")
    model_token, offset = _read_exact(raw, offset, token_length)
    if offset != len(raw):
        raise ValueError("trailing bytes in worker provisioning frame")
    try:
        metadata = json.loads(metadata_raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("metadata field is not valid JSON") from exc
    if not isinstance(metadata, dict):
        raise ValueError("metadata field must be a JSON object")
    return metadata, root_password, model_token


def _validate_output_dir(value: Any) -> Path:
    if not isinstance(value, (str, Path)) or not str(value):
        raise ValueError("output_dir is required")
    path = Path(value).expanduser()
    if not path.is_absolute() or any(part in (".", "..") for part in path.parts):
        raise ValueError("output_dir must be an absolute non-traversing path")
    current = path
    while True:
        if current.is_symlink():
            raise ValueError("output_dir must not traverse a symlink or reparse point")
        if current.exists() and getattr(current.lstat(), "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400):
            raise ValueError("output_dir must not traverse a symlink or reparse point")
        parent = current.parent
        if parent == current:
            break
        current = parent
    resolved = path.resolve(strict=False)
    if resolved.exists():
        raise ValueError("output_dir must be a new directory")
    return resolved


def _validate_metadata(metadata: dict[str, Any], *, require_output_dir: bool = False) -> None:
    _validate_metadata_fields(metadata)
    if "attempt_id" in metadata and not _safe_attempt_id(metadata["attempt_id"]):
        raise ValueError("attempt_id is invalid")
    if metadata.get("schema") != "worker-provision/1":
        raise ValueError("unsupported worker provisioning schema")
    if require_output_dir or "output_dir" in metadata:
        _validate_output_dir(metadata.get("output_dir"))
    host = metadata.get("host")
    if not isinstance(host, str) or not host or len(host) > 255:
        raise ValueError("host is required")
    try:
        address = ipaddress.ip_address(host)
    except ValueError as exc:
        raise ValueError("host must be a valid IP address") from exc
    if address.version != 4:
        raise ValueError("host must be an IPv4 address")
    root_user = metadata.get("root_user")
    if not isinstance(root_user, str) or not root_user or len(root_user) > MAX_USER or any(c.isspace() or ord(c) < 32 for c in root_user):
        raise ValueError("root_user is invalid")
    _validate_origin(metadata.get("model_base_url"))
    model = metadata.get("model")
    if not isinstance(model, str) or not model.strip() or len(model) > MAX_MODEL or any(ord(c) < 32 for c in model):
        raise ValueError("model is invalid")
    if "connect_addr" in metadata and metadata["connect_addr"] is not None:
        # Optional connect override from the Manager host's resolution; it is
        # validated again (against the pinned origin) by build_endpoint_config.
        connect = metadata["connect_addr"]
        if not isinstance(connect, str):
            raise ValueError("connect_addr must be an ip:port string")
        try:
            override_host, override_port_text = connect.rsplit(":", 1)
            override_port = int(override_port_text)
            override_address = ipaddress.ip_address(override_host)
        except ValueError as exc:
            raise ValueError("connect_addr must be an ip:port literal") from exc
        if override_address.version != 4 or not 1 <= override_port <= 65535:
            raise ValueError("connect_addr must be an IPv4:port literal")


def validate_request(metadata: dict[str, Any], root_password: bytes, model_token: bytes, *, require_output_dir: bool = False) -> None:
    _validate_metadata(metadata, require_output_dir=require_output_dir)
    if not isinstance(root_password, bytes) or not root_password or len(root_password) > MAX_SECRET:
        raise ValueError("root password is empty or oversized")
    if not isinstance(model_token, bytes) or not model_token or len(model_token) > MAX_SECRET:
        raise ValueError("model token is empty or oversized")


def build_endpoint_config(base_url: str, model: str, certificate_pem: str, *,
                          token: str | None = None,
                          connect_addr: str | None = None) -> dict[str, Any]:
    if token is not None:
        raise ValueError("endpoint configuration must not contain token")
    _validate_origin(base_url)
    if not isinstance(model, str) or not model.strip() or len(model) > MAX_MODEL:
        raise ValueError("model is invalid")
    if (not isinstance(certificate_pem, str)
            or not certificate_pem.startswith("-----BEGIN CERTIFICATE-----")
            or "-----END CERTIFICATE-----" not in certificate_pem):
        raise ValueError("certificate PEM is invalid")
    endpoint: dict[str, Any] = {
        "schema": "model-endpoint/1",
        "base_url": base_url,
        "model": model,
        "certificate_pem": certificate_pem,
        # 240 matches ModelEndpoint::MAX_TIMEOUT_SECS and the historical
        # working chain (dist/linux221-deploy-20261116-01): reasoning models
        # can generate past 120 s. 240 s + owner overhead stays inside the
        # 270 s gateway exchange window.
        "timeout_secs": 240,
    }
    if connect_addr is not None:
        # A connect override only steers TCP for the URL host; TLS SNI, name
        # verification, and the byte-for-byte pin stay bound to base_url. The
        # base_url keeps its DNS name (historical contract, linux221
        # 1152214d); the override itself must be a bare IPv4:port literal.
        try:
            override_host, override_port_text = connect_addr.rsplit(":", 1)
            override_port = int(override_port_text)
            address = ipaddress.ip_address(override_host)
        except ValueError as exc:
            raise ValueError("connect override must be an ip:port literal") from exc
        if address.version != 4 or not 1 <= override_port <= 65535:
            raise ValueError("connect override must be an IPv4:port literal")
        endpoint["connect_addr"] = f"{address}:{override_port}"
    return endpoint


def derive_connect_addr(base_url: str, resolved_ipv4: list[str] | tuple[str, ...]) -> str | None:
    """Return ``ip:port`` when the Manager host resolves a hostname to a
    private LAN address the public DNS does not serve.

    Mirrors the historical manual chain (dist/linux221-deploy-20261116-01):
    the Manager host's resolver is the view the user validated, so a private
    resolution becomes the worker's connect override. Public answers and IP
    literals are never overridden.
    """
    parsed = urlparse(base_url)
    host = parsed.hostname or ""
    if not host:
        return None
    try:
        ipaddress.ip_address(host)
        return None  # IP literal: resolution is not involved
    except ValueError:
        pass
    port = parsed.port or 443
    for answer in resolved_ipv4:
        try:
            address = ipaddress.ip_address(answer)
        except ValueError:
            continue
        if address.version == 4 and address.is_private:
            return f"{address}:{port}"
    return None


def _safe_linux_path(value: str) -> bool:
    return (isinstance(value, str) and value.startswith("/")
            and ".." not in value.split("/")
            and all(character.isalnum() or character in "/._-@" for character in value))


def _safe_sha256(value: Any) -> bool:
    return isinstance(value, str) and bool(re.fullmatch(r"[0-9a-f]{64}", value))


def _safe_asset_version(value: Any) -> bool:
    return isinstance(value, str) and bool(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._:-]{0,63}", value))


def _safe_attempt_id(value: Any) -> bool:
    return isinstance(value, str) and bool(re.fullmatch(r"attempt-[0-9a-f]{12,32}", value))


def _safe_remote_staging_root(value: Any) -> bool:
    return isinstance(value, str) and bool(
        re.fullmatch(r"/home/[A-Za-z0-9_-]+/cross-runtime-[A-Za-z0-9-]+", value)
    )


def _safe_reason_code(value: Any) -> bool:
    return isinstance(value, str) and bool(re.fullmatch(r"[a-z][a-z0-9_]{0,63}", value))


def _safe_identity(identity: dict[str, Any]) -> None:
    if not isinstance(identity, dict):
        raise ValueError("ordinary user identity is invalid")
    user = identity.get("user")
    home = identity.get("home")
    uid = identity.get("uid")
    gid = identity.get("gid")
    if (not isinstance(user, str) or not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", user)
            or home != f"/home/{user}" or type(uid) is not int
            or type(gid) is not int or not 1000 <= uid < 65534
            or not 0 <= gid < 65534):
        raise ValueError("ordinary user identity is invalid")


def build_internal_configs(
    *,
    output_dir: str | Path,
    deployment: str,
    worker_id: str,
    host: str,
    port: int,
    identity: dict[str, Any],
    known_hosts: str | Path,
    identity_file: str | Path,
    protocol_key_file: str | Path,
    remote_root: str,
    assets: dict[str, Any],
    endpoint: dict[str, Any],
) -> dict[str, Any]:
    """Build manager/worker/gateway JSON without mutating the filesystem.

    The returned paths are explicit and unique under ``output_dir``; secrets
    remain outside these configs and are supplied only through runtime memory.
    """
    if not isinstance(deployment, str) or not re.fullmatch(r"[a-z0-9][a-z0-9._:-]{0,127}", deployment) or ".." in deployment:
        raise ValueError("deployment id is invalid")
    if not isinstance(worker_id, str) or not re.fullmatch(r"[a-z0-9][a-z0-9._:-]{0,127}", worker_id) or ".." in worker_id:
        raise ValueError("worker id is invalid")
    try:
        address = ipaddress.ip_address(host)
    except ValueError as exc:
        raise ValueError("worker host must be an IP address") from exc
    if address.version != 4 or type(port) is not int or not 1 <= port <= 65535:
        raise ValueError("worker address is invalid")
    _safe_identity(identity)
    if not _safe_linux_path(remote_root) or remote_root.endswith("/"):
        raise ValueError("remote worker root is invalid")
    if not isinstance(known_hosts, (str, Path)) or not str(known_hosts):
        raise ValueError("known_hosts path is required")
    if not isinstance(identity_file, (str, Path)) or not str(identity_file):
        raise ValueError("ordinary identity path is required")
    if not isinstance(protocol_key_file, (str, Path)) or not str(protocol_key_file):
        raise ValueError("protocol key path is required")
    for name in ("worker", "loader"):
        entry = assets.get(name) if isinstance(assets, dict) else None
        if not isinstance(entry, dict) or not _safe_sha256(entry.get("sha256")) or type(entry.get("bytes")) is not int or entry["bytes"] <= 0:
            raise ValueError(f"{name} asset identity is invalid")
    if not isinstance(endpoint, dict) or endpoint.get("schema") != "model-endpoint/1" or _contains_secret_marker(endpoint):
        raise ValueError("model endpoint configuration is invalid")
    endpoint = json.loads(json.dumps(endpoint, ensure_ascii=False))
    output = Path(output_dir).expanduser().resolve()
    protocol = str(Path(protocol_key_file).expanduser().resolve())
    ordinary_key = str(Path(identity_file).expanduser().resolve())
    known = str(Path(known_hosts).expanduser().resolve())
    controller_path = str(output / "controller.json")
    worker_path = str(output / "worker.json")
    endpoint_path = str(output / "model-endpoint.json")
    gateway_path = str(output / "gateway.json")
    worker_asset = assets["worker"]
    loader_asset = assets["loader"]
    remote_worker = f"{remote_root}/aw-cli"
    remote_config = f"{remote_root}/worker.json"
    remote_library = f"{remote_root}/lib"
    controller = {
        "schema": "ao-reason-config/1", "mode": "real", "role": "controller",
        "deployment": deployment, "state_dir": str(output / "controller-state"),
        "nodes": [{"id": worker_id, "address": f"{host}:{port}", "key_file": protocol,
                   "slots": 1, "templates": ["sum-v1", "control-v1", "project-work-v1"],
                   "admitted": True, "healthy": False, "draining": False}],
    }
    worker = {
        "schema": "native-worker-config/1", "deployment": deployment, "worker_id": worker_id,
        "key_file": "protocol.key",
        "cgroup_root": f"/sys/fs/cgroup/user.slice/user-{identity['uid']}.slice/user@{identity['uid']}.service",
        "cgroup_root_mode": "current-unit", "artifact_sha256": worker_asset["sha256"],
        "timeout_secs": 270,
    }
    gateway = {
        "schema": "native-ssh-gateway/1", "deployment": deployment, "worker_id": worker_id,
        "host": host, "port": port, "user": identity["user"], "known_hosts": known,
        "identity_file": ordinary_key, "controller_config": controller_path, "key_file": protocol,
        "state_dir": str(output / "gateway-state"), "remote_worker": remote_worker,
        "remote_config": remote_config, "artifact_sha256": worker_asset["sha256"],
        "artifact_bytes": worker_asset["bytes"], "timeout_secs": 300,
        "launch_mode": "delegated-user-service", "expected_worker_timeout_secs": 270,
        "remote_library_path": remote_library, "remote_library_sha256": loader_asset["sha256"],
        "remote_library_bytes": loader_asset["bytes"],
    }
    return {"controller": controller, "worker": worker, "gateway": gateway,
            "endpoint": endpoint, "paths": {"controller": controller_path, "worker": worker_path,
            "endpoint": endpoint_path, "gateway": gateway_path, "protocol_key": protocol}}


def provision_worker(
    metadata: dict[str, Any],
    root_password: bytes,
    model_token: bytes,
    prerequisites: dict[str, str],
    install_root: Path | str,
    output_dir: Path | str,
    *,
    connector: Any = None,
    key_connector: Any = None,
    key_generator: Any = None,
    deployment: str | None = None,
    worker_id: str = "worker-b",
) -> tuple[dict[str, Any], bytes]:
    """Run the verified provisioning stages and return redacted result + key."""
    _validate_metadata(metadata, require_output_dir=True)
    attempt_id = metadata.get("attempt_id")
    if not _safe_attempt_id(attempt_id):
        raise ProvisionStageError("attempt_identity_invalid", "使用当前 Manager 尝试身份重新准备 Worker")
    controlled_output = _validate_output_dir(metadata["output_dir"])
    requested_output = Path(output_dir).expanduser().resolve()
    if requested_output != controlled_output:
        raise ProvisionStageError("output_dir_mismatch", "仅使用 Manager 传入的受控输出目录后重试")
    if not isinstance(root_password, bytes) or not root_password or len(root_password) > MAX_SECRET:
        raise ProvisionStageError("root_ssh_unavailable", "检查 root 凭据后重试")
    if not isinstance(model_token, bytes) or not model_token or len(model_token) > MAX_SECRET:
        raise ProvisionStageError("model_endpoint_invalid", "检查 API key 后重试")
    try:
        prepared = prepare_local_provisioning(metadata, prerequisites, install_root)
        access = bootstrap_ordinary_access(
            metadata, root_password, prepared["known_hosts"],
            key_generator=key_generator, connector=connector, key_connector=key_connector,
        )
        identity = access["identity"]
        deployment = deployment or f"worker-{uuid.uuid4().hex[:12]}"
        remote_root = f"{identity['home']}/cross-runtime-{uuid.uuid4().hex[:12]}"
        protocol_key = os.urandom(32)
        staged = stage_local_provisioning(
            output_dir, deployment, worker_id, metadata["host"], FIXED_PORT,
            identity=identity, known_hosts=prepared["known_hosts"], private_key=access["private_key"],
            protocol_key=protocol_key, remote_root=remote_root, assets=prepared["assets"],
            endpoint=prepared["endpoint"],
        )
        ordinary = (key_connector or connect_ordinary_ssh)(
            metadata["host"], identity["user"], prepared["known_hosts"],
            private_key=access["private_key"], port=FIXED_PORT, timeout=15,
        )
        try:
            upload = upload_staged_worker(
                ordinary,
                {
                    "aw-cli": prepared["assets"]["worker"]["path"],
                    "libsqlite3.so.0": prepared["assets"]["loader"]["path"],
                    "worker.json": Path(staged["output_dir"]) / "worker.json",
                    "model-endpoint.json": Path(staged["output_dir"]) / "model-endpoint.json",
                    "protocol.key": Path(staged["output_dir"]) / "protocol.key",
                },
                remote_root, identity,
                expected={
                    "aw-cli": prepared["assets"]["worker"],
                    "libsqlite3.so.0": prepared["assets"]["loader"],
                    "worker.json": {"bytes": Path(staged["output_dir"]).joinpath("worker.json").stat().st_size,
                                     "sha256": hashlib.sha256(Path(staged["output_dir"]).joinpath("worker.json").read_bytes()).hexdigest()},
                    "model-endpoint.json": {"bytes": Path(staged["output_dir"]).joinpath("model-endpoint.json").stat().st_size,
                                            "sha256": hashlib.sha256(Path(staged["output_dir"]).joinpath("model-endpoint.json").read_bytes()).hexdigest()},
                    "protocol.key": {"bytes": 65, "sha256": hashlib.sha256(protocol_key.hex().encode("ascii") + b"\n").hexdigest()},
                },
            )
            preflight = remote_preflight(ordinary, staged["configs"], identity, prepared["assets"])
        finally:
            ordinary.close()
        if upload.get("status") != "PASS_REMOTE_UPLOAD" or preflight.get("status") != "PASS_REMOTE_PREFLIGHT":
            raise ProvisionStageError("remote_preflight_failed", "远端上传或 preflight 未通过，请检查 Worker 后重试")
        result = redacted_result(
            "PASS_WORKER_PROVISION", failed_stage=None, error_type=None,
            next_step="Worker 已完成受控准备，可由 Manager 校验并激活",
            root_password=root_password, model_token=model_token,
            preflight=True, gateway_path=str(Path(staged["output_dir"]) / "gateway.json"),
            artifact_sha256=prepared["assets"]["worker"]["sha256"],
            artifact_bytes=prepared["assets"]["worker"]["bytes"], worker_id=worker_id,
            host=metadata["host"],
            attempt_id=attempt_id,
        )
        return result, access["private_key"]
    except (LocalPrerequisiteError, ProvisionStageError):
        raise
    except Exception as exc:
        raise ProvisionStageError("orchestration_failed", "检查最新阶段错误并保留现有配置后重试") from exc


def _write_json_exclusive(path: Path, value: dict[str, Any]) -> None:
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    fd = os.open(path, flags, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as stream:
            json.dump(value, stream, ensure_ascii=False, indent=2)
            stream.write("\n")
    except Exception:
        try:
            path.unlink()
        except OSError:
            pass
        raise


def _write_bytes_exclusive(path: Path, value: bytes) -> None:
    if not isinstance(value, bytes) or not value:
        raise ValueError("staged secret file is empty or invalid")
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    fd = os.open(path, flags, 0o600)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(value)
    except Exception:
        try:
            path.unlink()
        except OSError:
            pass
        raise


def stage_local_provisioning(
    output_dir: Path | str,
    deployment: str,
    worker_id: str,
    host: str,
    port: int,
    *,
    identity: dict[str, Any],
    known_hosts: Path | str,
    private_key: bytes,
    protocol_key: bytes,
    remote_root: str,
    assets: dict[str, Any],
    endpoint: dict[str, Any],
) -> dict[str, Any]:
    """Create one fresh local provisioning directory with exclusive writes."""
    requested_output = Path(output_dir).expanduser()
    if requested_output.exists():
        raise FileExistsError(requested_output)
    output = _validate_output_dir(requested_output)
    output.mkdir(parents=True, exist_ok=False)
    try:
        if not isinstance(protocol_key, bytes) or len(protocol_key) != 32:
            raise ValueError("protocol key must contain exactly 32 bytes")
        configs = build_internal_configs(
            output_dir=output, deployment=deployment, worker_id=worker_id,
            host=host, port=port, identity=identity, known_hosts=known_hosts,
            identity_file=output / "ordinary-user-rsa",
            protocol_key_file=output / "protocol.key", remote_root=remote_root,
            assets=assets, endpoint=endpoint,
        )
        # The ordinary SSH key is returned through the private result pipe only.
        # Config key_file readers on both platforms expect 32 bytes as hex text.
        _write_bytes_exclusive(output / "protocol.key", protocol_key.hex().encode("ascii") + b"\n")
        _write_json_exclusive(output / "controller.json", configs["controller"])
        _write_json_exclusive(output / "worker.json", configs["worker"])
        _write_json_exclusive(output / "model-endpoint.json", configs["endpoint"])
        _write_json_exclusive(output / "gateway.json", configs["gateway"])
        return {"output_dir": str(output), "configs": configs}
    except Exception:
        # The parent was required to be new; leave no partial staging behind.
        for child in output.iterdir():
            if child.is_file():
                child.unlink()
        output.rmdir()
        raise


def _attempt_id_for_output(output_dir: Path | str) -> str:
    name = Path(output_dir).name
    if name.startswith("worker-"):
        candidate = name[len("worker-"):]
        if _safe_attempt_id(candidate):
            return candidate
    return f"attempt-{uuid.uuid4().hex}"


def create_retry_plan(state_dir: Path | str) -> dict[str, str]:
    """Allocate append-only local retry identity without reading secrets or old output."""
    state = Path(state_dir).expanduser()
    if not state.is_absolute() or any(part in (".", "..") for part in state.parts):
        raise ValueError("state_dir must be an absolute non-traversing path")
    current = state
    while True:
        if current.is_symlink():
            raise ValueError("state_dir must not traverse a symlink or reparse point")
        if current.exists() and getattr(current.lstat(), "st_file_attributes", 0) & getattr(stat, "FILE_ATTRIBUTE_REPARSE_POINT", 0x400):
            raise ValueError("state_dir must not traverse a symlink or reparse point")
        parent = current.parent
        if parent == current:
            break
        current = parent
    provisioning = state / "provisioning"
    provisioning.mkdir(parents=True, exist_ok=True)
    attempt_id = f"attempt-{uuid.uuid4().hex}"
    output = provisioning / f"worker-{attempt_id}"
    if output.exists():
        raise FileExistsError(output)
    return {"attempt_id": attempt_id, "output_dir": str(output)}


def _remote_asset_paths(remote_root: str) -> dict[str, str]:
    return {
        "aw-cli": f"{remote_root}/aw-cli",
        "libsqlite3.so.0": f"{remote_root}/lib/libsqlite3.so.0",
        "worker.json": f"{remote_root}/worker.json",
        "model-endpoint.json": f"{remote_root}/model-endpoint.json",
        "protocol.key": f"{remote_root}/protocol.key",
    }


def _remote_version_path(remote_root: str) -> str:
    return f"{remote_root}/.asset-version"


def _remote_identity(
    client: Any,
    remote_root: str,
    identity: dict[str, Any],
    modes: dict[str, int],
    expected: dict[str, dict[str, Any]],
    names: tuple[str, ...],
    asset_version: str | None,
    *,
    timeout: float,
) -> dict[str, dict[str, Any]]:
    remote_paths = _remote_asset_paths(remote_root)
    checks = []
    for name in names:
        remote = remote_paths[name]
        checks.extend((
            f"stat -c '%u %g %a %s %n' {shlex.quote(remote)}",
            f"sha256sum {shlex.quote(remote)}",
        ))
    version_path = _remote_version_path(remote_root)
    if asset_version is not None:
        checks.extend((
            f"stat -c '%u %g %a %s %n' {shlex.quote(version_path)}",
            f"cat -- {shlex.quote(version_path)}",
        ))
    stdin, stdout, stderr = client.exec_command("set -eu; " + "; ".join(checks), timeout=timeout)
    try:
        stdin.close()
        output = stdout.read(MAX_RESULT + 1)
        error = stderr.read(MAX_RESULT + 1)
        code = stdout.channel.recv_exit_status()
    finally:
        stdout.close()
        stderr.close()
    if code != 0 or error or len(output) > MAX_RESULT or len(error) > MAX_RESULT:
        raise ValueError("remote asset identity command failed")
    lines = [line.strip() for line in output.decode("utf-8").splitlines() if line.strip()]
    expected_lines = len(names) * 2 + (2 if asset_version is not None else 0)
    if len(lines) != expected_lines:
        raise ValueError("remote asset identity output shape mismatch")
    remote_identity: dict[str, dict[str, Any]] = {}
    for index, name in enumerate(names):
        remote = remote_paths[name]
        stat_fields = lines[index * 2].split(" ", 4)
        hash_fields = lines[index * 2 + 1].split(None, 1)
        if len(stat_fields) != 5 or len(hash_fields) != 2:
            raise ValueError("remote asset identity fields are invalid")
        if stat_fields[4] != remote or hash_fields[1] != remote:
            raise ValueError("remote asset identity path mismatch")
        record = {
            "uid": int(stat_fields[0]), "gid": int(stat_fields[1]),
            "mode": int(stat_fields[2], 8), "bytes": int(stat_fields[3]),
            "sha256": hash_fields[0],
        }
        expected_record = expected[name]
        if (record["uid"] != identity["uid"] or record["gid"] != identity["gid"]
                or record["mode"] != modes[name] or record["bytes"] != expected_record["bytes"]
                or record["sha256"] != expected_record["sha256"]):
            raise ValueError(f"remote asset identity mismatch: {name}")
        remote_identity[name] = record
    if asset_version is not None:
        version_stat = lines[-2].split(" ", 4)
        if len(version_stat) != 5 or version_stat[4] != version_path:
            raise ValueError("remote asset version path mismatch")
        if (int(version_stat[0]) != identity["uid"] or int(version_stat[1]) != identity["gid"]
                or int(version_stat[2], 8) != 0o600 or lines[-1] != asset_version):
            raise ValueError("remote asset version identity mismatch")
    return remote_identity


def upload_staged_worker(
    client: Any,
    local_files: dict[str, Path | str],
    remote_root: str,
    identity: dict[str, Any],
    *,
    expected: dict[str, dict[str, Any]],
    timeout: float = 30.0,
    asset_version: str | None = None,
    reuse_remote_root: str | None = None,
) -> dict[str, Any]:
    """Upload a fresh owner-owned tree, or reuse an explicitly verified tree."""
    _safe_identity(identity)
    if not _safe_linux_path(remote_root) or remote_root.endswith("/"):
        raise ProvisionStageError("remote_upload_invalid", "重新生成安全的远端 staging 路径后重试")
    if asset_version is not None and not _safe_asset_version(asset_version):
        raise ProvisionStageError("remote_upload_invalid", "安装匹配版本的 Worker 资产后重试")
    names = ("aw-cli", "libsqlite3.so.0", "worker.json", "model-endpoint.json", "protocol.key")
    if set(local_files) != set(names) or set(expected) != set(names):
        raise ProvisionStageError("remote_upload_invalid", "本地上传清单不完整或包含未知文件")
    modes = {
        "aw-cli": 0o700,
        "libsqlite3.so.0": 0o700,
        "worker.json": 0o600,
        "model-endpoint.json": 0o600,
        "protocol.key": 0o600,
    }
    checked: dict[str, dict[str, Any]] = {}
    try:
        for name in names:
            path = Path(local_files[name]).expanduser().resolve()
            if not path.is_file():
                raise ValueError(f"local upload file is missing: {name}")
            content = path.read_bytes()
            digest = hashlib.sha256(content).hexdigest()
            record = expected[name]
            if (type(record.get("bytes")) is not int or record["bytes"] != len(content)
                    or record.get("sha256") != digest
                    or ("version" in record and record["version"] != asset_version)):
                raise ValueError(f"local upload identity mismatch: {name}")
            checked[name] = {"bytes": len(content), "sha256": digest}
    except Exception as exc:
        raise ProvisionStageError("remote_upload_failed", "检查远端目录权限、文件 owner/mode 和资产 hash 后重试") from exc
    selected_root = remote_root
    if reuse_remote_root is not None:
        if (not _safe_linux_path(reuse_remote_root) or reuse_remote_root.endswith("/")
                or not reuse_remote_root.startswith(identity["home"] + "/cross-runtime-")):
            raise ProvisionStageError("remote_upload_invalid", "仅可复用普通用户目录下的显式 staging 路径")
        try:
            existing = _remote_identity(
                client, reuse_remote_root, identity, modes, expected, names, asset_version,
                timeout=timeout,
            )
            return {
                "status": "PASS_REMOTE_UPLOAD", "remote_root": reuse_remote_root,
                "files": {
                    name: {**record, "reused": True, "uploaded": False,
                           "asset_version": asset_version}
                    for name, record in existing.items()
                },
            }
        except Exception:
            selected_root = f"{identity['home']}/cross-runtime-{uuid.uuid4().hex[:12]}"
    remote_paths = _remote_asset_paths(selected_root)
    remote_mutation_started = False
    try:
        command = (
            "set -eu; umask 077; "
            f"mkdir {shlex.quote(selected_root)}; mkdir {shlex.quote(selected_root + '/lib')}; "
            f"chmod 700 {shlex.quote(selected_root)} {shlex.quote(selected_root + '/lib')}"
        )
        remote_mutation_started = True
        stdin, stdout, stderr = client.exec_command(command, timeout=timeout)
        stdin.close()
        code = stdout.channel.recv_exit_status()
        error = stderr.read(MAX_RESULT + 1)
        stdout.close()
        stderr.close()
        if code != 0 or error:
            raise ValueError("remote staging directory creation failed")
        sftp = client.open_sftp()
        try:
            for name in names:
                remote = remote_paths[name]
                sftp.put(str(Path(local_files[name]).expanduser().resolve()), remote)
                sftp.chmod(remote, modes[name])
        finally:
            sftp.close()
        if asset_version is not None:
            version_path = _remote_version_path(selected_root)
            command = (
                "set -eu; umask 077; "
                f"printf '%s\\n' {shlex.quote(asset_version)} > {shlex.quote(version_path)}; "
                f"chmod 600 {shlex.quote(version_path)}"
            )
            stdin, stdout, stderr = client.exec_command(command, timeout=timeout)
            stdin.close()
            code = stdout.channel.recv_exit_status()
            error = stderr.read(MAX_RESULT + 1)
            stdout.close()
            stderr.close()
            if code != 0 or error:
                raise ValueError("remote asset version write failed")
        remote_identity = _remote_identity(
            client, selected_root, identity, modes, expected, names, asset_version,
            timeout=timeout,
        )
        return {
            "status": "PASS_REMOTE_UPLOAD", "remote_root": selected_root,
            "files": {
                name: {**record, "reused": False, "uploaded": True,
                       "asset_version": asset_version}
                for name, record in remote_identity.items()
            },
        }
    except ProvisionStageError:
        raise
    except Exception as exc:
        diagnostics = {
            "stage": "remote_upload",
            "remote_root": selected_root,
            "remote_side_effects": "present" if remote_mutation_started else "absent",
            "reason_code": "remote_upload_failed",
        }
        raise ProvisionStageError(
            "remote_upload_failed", "检查远端目录权限、文件 owner/mode 和资产 hash 后重试", diagnostics
        ) from exc


def remote_preflight(
    client: Any,
    configs: dict[str, Any],
    identity: dict[str, Any],
    assets: dict[str, Any],
    *,
    timeout: float = 30.0,
) -> dict[str, Any]:
    """Run bounded, read-only owner/cgroup/artifact checks on the worker."""
    _safe_identity(identity)
    if not isinstance(configs, dict) or not isinstance(assets, dict):
        raise ProvisionStageError("remote_preflight_invalid", "检查本地配置和资产清单后重试")
    worker = configs.get("worker")
    gateway = configs.get("gateway")
    if not isinstance(worker, dict) or not isinstance(gateway, dict):
        raise ProvisionStageError("remote_preflight_invalid", "配置缺少 Worker 或 Gateway 身份后重试")
    remote_worker = gateway.get("remote_worker")
    remote_library = gateway.get("remote_library_path")
    cgroup = worker.get("cgroup_root")
    if not all(isinstance(value, str) and _safe_linux_path(value) for value in (remote_worker, remote_library, cgroup)):
        raise ProvisionStageError("remote_preflight_invalid", "远端路径不安全，重新生成配置后重试")
    worker_asset = assets.get("worker", {})
    loader_asset = assets.get("loader", {})
    command = (
        "set -eu; "
        f"test \"$(id -u)\" = {identity['uid']}; "
        f"test \"$(id -g)\" = {identity['gid']}; "
        f"test \"$(id -un)\" = {shlex.quote(identity['user'])}; "
        f"test \"$(sha256sum {shlex.quote(remote_worker)} | cut -d' ' -f1)\" = {worker_asset['sha256']}; "
        f"test \"$(wc -c < {shlex.quote(remote_worker)})\" -eq {worker_asset['bytes']}; "
        f"test \"$(sha256sum {shlex.quote(remote_library + '/libsqlite3.so.0')} | cut -d' ' -f1)\" = {loader_asset['sha256']}; "
        f"test \"$(wc -c < {shlex.quote(remote_library + '/libsqlite3.so.0')})\" -eq {loader_asset['bytes']}; "
        f"test -d {shlex.quote(cgroup)}; "
        f"test -r {shlex.quote(cgroup + '/cgroup.controllers')}; "
        f"test -w {shlex.quote(cgroup + '/cgroup.procs')}; "
        f"LD_LIBRARY_PATH={shlex.quote(remote_library)} {shlex.quote(remote_worker)} --identity; "
        f"LD_LIBRARY_PATH={shlex.quote(remote_library)} {shlex.quote(remote_worker)} --help; "
        f"LD_LIBRARY_PATH={shlex.quote(remote_library)} {shlex.quote(remote_worker)} --capabilities"
    )
    if len(command) > MAX_RESULT:
        raise ProvisionStageError("remote_preflight_invalid", "远端 preflight 命令超出大小限制")
    try:
        stdin, stdout, stderr = client.exec_command(command, timeout=timeout)
        stdin.close()
        output = stdout.read(MAX_RESULT + 1)
        error = stderr.read(MAX_RESULT + 1)
        code = stdout.channel.recv_exit_status()
        stdout.close()
        stderr.close()
    except Exception as exc:
        raise ProvisionStageError("remote_preflight_failed", "检查远端 Worker、cgroup 和 CLI 后重试") from exc
    if len(output) > MAX_RESULT or len(error) > MAX_RESULT or code != 0 or error:
        raise ProvisionStageError("remote_preflight_failed", "检查远端 Worker、cgroup 和 CLI 后重试")
    try:
        text = output.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ProvisionStageError("remote_preflight_failed", "远端 preflight 输出不是 UTF-8") from exc
    if '"binary":"aw-cli"' not in text or "owner-exec --config FILE" not in text:
        raise ProvisionStageError("remote_cli_identity_invalid", "安装匹配版本的 Worker CLI 后重试")
    try:
        capability = json.loads(text.rstrip().splitlines()[-1])
    except (ValueError, IndexError):
        capability = None
    if (not isinstance(capability, dict)
            or capability.get("schema") != "worker-capabilities/1"
            or capability.get("binary") != "aw-cli"
            or capability.get("platform") != "linux"
            or not isinstance(capability.get("owner_exec_templates"), list)
            or "project-work-v1" not in capability["owner_exec_templates"]
            or capability.get("model_token_frame") != "u32be-utf8/1"
            or capability.get("live_output") != "AO-LIVE/1"):
        raise ProvisionStageError(
            "remote_worker_capability_unsupported",
            "当前 Linux Worker 不支持项目执行或实时输出；请使用包含新版 Linux Worker 的完整程序包重新准备",
        )
    return {"status": "PASS_REMOTE_PREFLIGHT", "owner_exec_supported": True,
            "project_work_supported": True, "live_output_supported": True, "stdout": text}


def _contains_secret_marker(value: Any) -> bool:
    if isinstance(value, dict):
        return any(
            any(marker in str(key).lower() for marker in _SECRET_FIELD_MARKERS)
            or _contains_secret_marker(item)
            for key, item in value.items()
        )
    if isinstance(value, (list, tuple)):
        return any(_contains_secret_marker(item) for item in value)
    return False


def _safe_result_text(value: str | None, fallback: str, secrets: tuple[bytes, ...]) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str) or any(ord(c) < 32 for c in value):
        raise ValueError("provision result text is invalid")
    lowered = value.lower()
    if any(marker in lowered for marker in _SECRET_FIELD_MARKERS) or any(
        secret and secret.decode("utf-8", errors="ignore") in value for secret in secrets
    ):
        return fallback
    return value


def redacted_result(status: str, *, failed_stage: str | None = None, error_type: str | None = None,
                    next_step: str | None = None, root_password: bytes = b"", model_token: bytes = b"", **facts: Any) -> dict[str, Any]:
    if _contains_secret_marker(facts):
        raise ValueError("provision result contains secret fields")
    if "attempt_id" in facts and not _safe_attempt_id(facts["attempt_id"]):
        raise ValueError("attempt_id is invalid")
    secrets = (root_password, model_token)
    safe_failed_stage = _safe_result_text(failed_stage, "input", secrets)
    safe_error_type = _safe_result_text(error_type, "provision_error", secrets)
    safe_next_step = _safe_result_text(next_step, "检查输入后重试", secrets)
    result: dict[str, Any] = {"schema": "worker-provision/1", "status": status}
    if safe_failed_stage is not None:
        result["failed_stage"] = safe_failed_stage
    if safe_error_type is not None:
        result["error_type"] = safe_error_type
    if safe_next_step is not None:
        result["next_step"] = safe_next_step
    result.update(facts)
    encoded = json.dumps(result, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(encoded) > MAX_RESULT:
        raise ValueError("provision result is oversized")
    return result


def clear_buffers(*buffers: bytearray) -> None:
    for buffer in buffers:
        if not isinstance(buffer, bytearray):
            raise TypeError("secret buffers must be bytearray instances")
        buffer[:] = b"\x00" * len(buffer)


def load_assets(manifest_path: Path | str, expected_version: str) -> dict[str, Any]:
    manifest_file = Path(manifest_path).expanduser().resolve()
    try:
        manifest = json.loads(manifest_file.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("provision asset manifest is unreadable") from exc
    if not isinstance(manifest, dict) or manifest.get("schema") != "worker-provision-assets/1":
        raise ValueError("unsupported provision asset manifest")
    if manifest.get("version") != expected_version:
        raise ValueError("provision asset version mismatch")
    checked: dict[str, Any] = {"schema": manifest["schema"], "version": manifest["version"]}
    for name in ("worker", "loader"):
        entry = manifest.get(name)
        if not isinstance(entry, dict) or set(entry) != {"path", "bytes", "sha256"}:
            raise ValueError(f"invalid {name} asset manifest")
        relative = entry["path"]
        if not isinstance(relative, str) or not relative or Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise ValueError(f"invalid {name} asset path")
        asset_path = (manifest_file.parent / relative).resolve()
        try:
            asset_path.relative_to(manifest_file.parent.resolve())
            content = asset_path.read_bytes()
        except (OSError, ValueError) as exc:
            raise ValueError(f"{name} asset is unavailable") from exc
        digest = hashlib.sha256(content).hexdigest()
        if entry["bytes"] != len(content) or entry["sha256"] != digest:
            raise ValueError(f"{name} asset hash mismatch")
        checked[name] = {"path": str(asset_path), "bytes": len(content), "sha256": digest, "content": content}
    return checked


class LocalPrerequisiteError(ValueError):
    def __init__(self, error_type: str, next_step: str):
        super().__init__(error_type)
        self.error_type = error_type
        self.next_step = next_step


def _descriptor_path(install_root: Path | str) -> Path:
    root = Path(install_root).expanduser().resolve()
    return root / PREREQUISITES_FILENAME


def load_prerequisites_descriptor(path: Path | str) -> dict[str, str]:
    descriptor_path = Path(path).expanduser().resolve()
    try:
        descriptor = json.loads(descriptor_path.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise LocalPrerequisiteError(
            "prerequisites_descriptor_unavailable",
            "安装 provisioning-prerequisites.json 后重试",
        ) from exc
    expected = {"schema", "asset_manifest", "asset_version", "known_hosts", "tls_certificate"}
    if (not isinstance(descriptor, dict) or set(descriptor) != expected
            or descriptor.get("schema") != PREREQUISITES_SCHEMA):
        raise LocalPrerequisiteError(
            "prerequisites_descriptor_invalid",
            "修复 provisioning-prerequisites.json 的 schema 和字段后重试",
        )
    for field in expected - {"schema"}:
        value = descriptor[field]
        if not isinstance(value, str) or not value or Path(value).is_absolute() or ".." in Path(value).parts:
            raise LocalPrerequisiteError(
                "prerequisites_descriptor_invalid",
                "将前置条件路径改为安装目录内的相对路径后重试",
            )
    return descriptor


def _resolve_descriptor_file(base: Path, relative: str, error_type: str, next_step: str) -> Path:
    path = (base / relative).resolve()
    try:
        path.relative_to(base.resolve())
    except ValueError as exc:
        raise LocalPrerequisiteError(error_type, next_step) from exc
    if not path.is_file():
        raise LocalPrerequisiteError(error_type, next_step)
    return path


def prepare_local_provisioning(
    metadata: dict[str, Any],
    prerequisites: dict[str, str],
    install_root: Path | str,
) -> dict[str, Any]:
    """Read and verify local provisioning inputs without network or mutation."""
    _validate_metadata(metadata)
    descriptor = load_prerequisites_descriptor_from_mapping(prerequisites)
    root = Path(install_root).expanduser().resolve()
    manifest = _resolve_descriptor_file(
        root, descriptor["asset_manifest"], "assets_unavailable",
        "安装匹配版本的 provisioning-assets 清单后重试",
    )
    try:
        assets = load_assets(manifest, descriptor["asset_version"])
    except (OSError, ValueError) as exc:
        raise LocalPrerequisiteError(
            "assets_unavailable",
            "安装匹配版本且校验通过的 Worker 资产后重试",
        ) from exc
    known_hosts = _resolve_descriptor_file(
        root, descriptor["known_hosts"], "known_hosts_unavailable",
        "提供安装目录内的已审核 known_hosts 文件后重试",
    )
    certificate_path = _resolve_descriptor_file(
        root, descriptor["tls_certificate"], "tls_certificate_unavailable",
        "提供安装目录内的受信 TLS 证书 PEM 后重试",
    )
    try:
        certificate = certificate_path.read_text(encoding="ascii")
        endpoint = build_endpoint_config(
            metadata["model_base_url"], metadata["model"], certificate,
            connect_addr=metadata.get("connect_addr"))
    except (OSError, UnicodeError, ValueError) as exc:
        raise LocalPrerequisiteError(
            "tls_certificate_unavailable",
            "提供有效的受信 TLS 证书 PEM 后重试",
        ) from exc
    return {"assets": assets, "known_hosts": str(known_hosts), "endpoint": endpoint}


def load_prerequisites_descriptor_from_mapping(prerequisites: dict[str, str]) -> dict[str, str]:
    expected = {"schema", "asset_manifest", "asset_version", "known_hosts", "tls_certificate"}
    if (not isinstance(prerequisites, dict) or set(prerequisites) != expected
            or prerequisites.get("schema") != PREREQUISITES_SCHEMA):
        raise LocalPrerequisiteError(
            "prerequisites_descriptor_invalid",
            "修复 provisioning-prerequisites.json 的 schema 和字段后重试",
        )
    for field in expected - {"schema"}:
        value = prerequisites[field]
        if not isinstance(value, str) or not value or Path(value).is_absolute() or ".." in Path(value).parts:
            raise LocalPrerequisiteError(
                "prerequisites_descriptor_invalid",
                "将前置条件路径改为安装目录内的相对路径后重试",
            )
    return dict(prerequisites)


RESULT_MAGIC = b"AW-WORKER-PROVISION-RESULT/1\n"


def encode_result_frame(metadata: dict[str, Any], private_key: bytes = b"") -> bytes:
    if not isinstance(metadata, dict) or _contains_secret_marker(metadata):
        raise ValueError("provision result metadata contains secret fields")
    if not isinstance(private_key, bytes) or len(private_key) > MAX_PRIVATE_KEY:
        raise ValueError("private key field is oversized")
    encoded = json.dumps(metadata, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")
    if len(encoded) > MAX_RESULT:
        raise ValueError("provision result metadata is oversized")
    frame = RESULT_MAGIC + struct.pack(">I", len(encoded)) + encoded
    frame += struct.pack(">I", len(private_key)) + private_key
    if len(frame) > MAX_RESULT_FRAME:
        raise ValueError("provision result frame is oversized")
    return frame


def decode_result_frame(raw: bytes) -> tuple[dict[str, Any], bytes]:
    if not isinstance(raw, bytes) or len(raw) > MAX_RESULT_FRAME or not raw.startswith(RESULT_MAGIC):
        raise ValueError("invalid worker provisioning result frame")
    offset = len(RESULT_MAGIC)
    metadata_length_raw, offset = _read_exact(raw, offset, 4)
    metadata_length = struct.unpack(">I", metadata_length_raw)[0]
    if metadata_length > MAX_RESULT:
        raise ValueError("provision result metadata is oversized")
    metadata_raw, offset = _read_exact(raw, offset, metadata_length)
    key_length_raw, offset = _read_exact(raw, offset, 4)
    key_length = struct.unpack(">I", key_length_raw)[0]
    if key_length > MAX_PRIVATE_KEY:
        raise ValueError("private key field is oversized")
    private_key, offset = _read_exact(raw, offset, key_length)
    if offset != len(raw):
        raise ValueError("trailing bytes in worker provisioning result frame")
    try:
        metadata = json.loads(metadata_raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError("provision result metadata is not valid JSON") from exc
    if not isinstance(metadata, dict) or _contains_secret_marker(metadata):
        raise ValueError("provision result metadata contains secret fields")
    return metadata, private_key


def create_unique_staging(parent: Path | str, prefix: str = "worker") -> Path:
    if not isinstance(prefix, str) or not prefix or prefix in {".", ".."} or any(c in prefix for c in "/\\") or ".." in prefix:
        raise ValueError("staging prefix is invalid")
    root = Path(parent).expanduser().resolve()
    root.mkdir(parents=True, exist_ok=True)
    for _ in range(32):
        candidate = root / f"{prefix}-{time.time_ns():x}-{os.getpid():x}"
        try:
            candidate.mkdir()
            return candidate
        except FileExistsError:
            time.sleep(0)
    raise RuntimeError("unable to allocate unique provisioning staging directory")


def _identity_lines(client: Any, command: str) -> list[str]:
    stdin, stdout, stderr = client.exec_command(command, timeout=15)
    try:
        output = stdout.read(4097)
        error = stderr.read(4097)
        if len(output) > 4096 or len(error) > 4096 or error:
            raise ValueError("remote identity output is invalid")
        if stdout.channel.recv_exit_status() != 0:
            raise ValueError("remote identity command failed")
        return output.decode("utf-8").splitlines()
    finally:
        stdin.close()
        stdout.close()
        stderr.close()
        stdout.channel.close()


class ProvisionStageError(ValueError):
    """Safe, redacted error for one provisioning stage."""

    def __init__(self, error_type: str, next_step: str, diagnostics: dict[str, Any] | None = None):
        super().__init__(error_type)
        self.error_type = error_type
        self.next_step = next_step
        self.diagnostics = dict(diagnostics or {})
        if _contains_secret_marker(self.diagnostics):
            raise ValueError("provision diagnostics contain secret fields")
        if any(not isinstance(value, (str, int, bool)) or isinstance(value, (str, bytes)) and len(value) > 512
               for value in self.diagnostics.values()):
            raise ValueError("provision diagnostics are invalid")


def verify_root_identity(client: Any) -> bool:
    return _identity_lines(client, "id -u") == ["0"]


def bootstrap_root_identity(
    metadata: dict[str, Any],
    root_password: bytes,
    known_hosts: Path | str,
    *,
    connector: Any = None,
    port: int = FIXED_PORT,
    timeout: float = 15.0,
) -> dict[str, Any]:
    """Authenticate as root, then discover exactly one ordinary user.

    This stage is deliberately read-only on the remote host. The caller owns
    later key installation and deployment stages.
    """
    try:
        _validate_metadata(metadata)
        if not isinstance(root_password, bytes) or not root_password or len(root_password) > MAX_SECRET:
            raise ValueError("root password is empty or oversized")
        connect = connector or connect_ssh
        client = connect(
            metadata["host"], metadata["root_user"], known_hosts,
            password=root_password, port=port, timeout=timeout,
        )
    except ProvisionStageError:
        raise
    except Exception as exc:
        raise ProvisionStageError(
            "root_ssh_unavailable", "检查 root 凭据、可信 known_hosts 和 SSH 服务后重试"
        ) from exc
    try:
        if not verify_root_identity(client):
            raise ProvisionStageError(
                "root_identity_invalid", "使用 root 账号重新连接；当前账号不是 UID 0"
            )
        try:
            return discover_ordinary_user(client)
        except ProvisionStageError:
            raise
        except Exception as exc:
            raise ProvisionStageError(
                "ordinary_user_discovery_failed", "确保 Linux 上存在且仅有一个可用普通用户后重试"
            ) from exc
    finally:
        client.close()


def connect_ordinary_ssh(
    host: str,
    user: str,
    known_hosts: Path | str,
    *,
    private_key: bytes,
    port: int = FIXED_PORT,
    timeout: float = 15.0,
):
    """Connect with a generated private key and explicit host-key trust."""
    if not isinstance(private_key, bytes) or not private_key or len(private_key) > MAX_PRIVATE_KEY:
        raise ValueError("ordinary private key is empty or oversized")
    try:
        import paramiko
        key = paramiko.RSAKey.from_private_key(io.StringIO(private_key.decode("ascii")))
    except (UnicodeDecodeError, ValueError, ImportError) as exc:
        raise ValueError("ordinary private key is invalid") from exc
    client = paramiko.SSHClient()
    client.load_host_keys(str(Path(known_hosts).expanduser().resolve()))
    client.set_missing_host_key_policy(paramiko.RejectPolicy())
    try:
        client.connect(
            hostname=host, port=port, username=user, pkey=key,
            password=None, allow_agent=False, look_for_keys=False,
            timeout=timeout, auth_timeout=timeout, banner_timeout=timeout,
        )
    except Exception:
        client.close()
        raise
    return client


def bootstrap_ordinary_access(
    metadata: dict[str, Any],
    root_password: bytes,
    known_hosts: Path | str,
    *,
    identity: dict[str, Any] | None = None,
    key_generator: Any = None,
    connector: Any = None,
    key_connector: Any = None,
    port: int = FIXED_PORT,
    timeout: float = 15.0,
) -> dict[str, Any]:
    """Install and verify a generated ordinary-user SSH key."""
    _validate_metadata(metadata)
    if not isinstance(root_password, bytes) or not root_password or len(root_password) > MAX_SECRET:
        raise ProvisionStageError("root_ssh_unavailable", "检查 root 凭据后重试")
    root_client = None
    failed_stage = "root_ssh_unavailable"
    next_step = "检查 root 凭据、可信 known_hosts 和 SSH 服务后重试"
    try:
        root_client = (connector or connect_ssh)(
            metadata["host"], metadata["root_user"], known_hosts,
            password=root_password, port=port, timeout=timeout,
        )
        if not verify_root_identity(root_client):
            raise ProvisionStageError(
                "root_identity_invalid", "使用 root 账号重新连接；当前账号不是 UID 0"
            )
        failed_stage = "ordinary_user_discovery_failed"
        next_step = "无法确定唯一普通用户；未执行目录初始化，请检查 Linux 账号配置"
        discovered = discover_ordinary_user(root_client)
        if identity is not None and discovered != identity:
            raise ProvisionStageError(
                "ordinary_user_identity_mismatch", "远端普通用户身份发生变化，请重新发现后重试"
            )
        identity = discovered
        failed_stage = "ordinary_key_install_failed"
        next_step = "普通用户目录与公钥自动初始化未完成；保留账本和原有公钥，检查远端文件类型或存储故障后重新准备"
        private_key, public_line = (key_generator or generate_worker_key)()
        sftp = root_client.open_sftp()
        try:
            install_public_key(sftp, public_line, identity=identity)
        finally:
            sftp.close()
    except ProvisionStageError:
        raise
    except Exception as exc:
        raise ProvisionStageError(failed_stage, next_step) from exc
    finally:
        if root_client is not None:
            root_client.close()
    try:
        ordinary_client = (key_connector or connect_ordinary_ssh)(
            metadata["host"], identity["user"], known_hosts,
            private_key=private_key, port=port, timeout=timeout,
        )
        try:
            if _identity_lines(ordinary_client, "id -u") != [str(identity["uid"])] or _identity_lines(ordinary_client, "id -un") != [identity["user"]]:
                raise ProvisionStageError(
                    "ordinary_key_login_invalid", "普通用户密钥登录身份不匹配，请检查公钥安装后重试"
                )
        finally:
            ordinary_client.close()
    except ProvisionStageError:
        raise
    except Exception as exc:
        raise ProvisionStageError(
            "ordinary_key_login_failed", "确认普通用户密钥登录和 known_hosts 后重试"
        ) from exc
    return {"identity": identity, "private_key": private_key, "public_key": public_line}


def discover_ordinary_user(client: Any) -> dict[str, Any]:
    candidates = []
    for line in _identity_lines(client, "getent passwd"):
        fields = line.split(":")
        if len(fields) != 7:
            raise ValueError("remote account entry is invalid")
        user, _, uid, gid, _, home, shell = fields
        if not uid.isdecimal() or not gid.isdecimal():
            raise ValueError("remote account identity is invalid")
        if not 1000 <= int(uid) < 65534 or shell not in ("/bin/bash", "/bin/sh", "/bin/zsh"):
            continue
        if not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", user) or home != f"/home/{user}":
            continue
        candidates.append({"user": user, "uid": int(uid), "gid": int(gid), "home": home})
    if len(candidates) != 1:
        raise ValueError("ordinary user identity is not unique")
    return candidates[0]


def generate_worker_key() -> tuple[bytes, str]:
    import paramiko

    key = paramiko.RSAKey.generate(2048)
    private_stream = io.StringIO()
    key.write_private_key(private_stream)
    private_bytes = private_stream.getvalue().encode("ascii")
    public_line = f"{key.get_name()} {key.get_base64()}"
    if len(private_bytes) > MAX_PRIVATE_KEY:
        raise ValueError("generated private key is oversized")
    return private_bytes, public_line


def install_public_key(
    sftp: Any,
    public_line: str,
    *,
    identity: dict[str, Any] | None = None,
    uid: int = 1000,
    gid: int = 1000,
) -> str:
    if (not isinstance(public_line, str) or not public_line.startswith("ssh-rsa ")
            or len(public_line) > 16 * 1024 or any(c in public_line for c in ("\r", "\n", "\x00"))):
        raise ValueError("invalid public key line")
    if identity is not None:
        if not isinstance(identity, dict):
            raise ValueError("ordinary user identity is invalid")
        user = identity.get("user")
        home = identity.get("home")
        discovered_uid = identity.get("uid")
        discovered_gid = identity.get("gid")
        if (not isinstance(user, str) or not re.fullmatch(r"[a-z_][a-z0-9_-]{0,31}", user)
                or home != f"/home/{user}" or type(discovered_uid) is not int
                or type(discovered_gid) is not int or not 1000 <= discovered_uid < 65534
                or not 0 <= discovered_gid < 65534):
            raise ValueError("ordinary user identity is invalid")
        uid, gid = discovered_uid, discovered_gid
    if type(uid) is not int or type(gid) is not int or not 1000 <= uid < 65534 or not 0 <= gid < 65534:
        raise ValueError("authorization owner identity is invalid")
    raw_line = public_line.encode("ascii")
    home = identity["home"] if identity is not None else "/home/ubuntu"
    ssh_dir = f"{home}/.ssh"
    authorized = f"{ssh_dir}/authorized_keys"
    parent = sftp.lstat("/home")
    if not stat.S_ISDIR(parent.st_mode) or parent.st_uid != 0 or parent.st_mode & 0o022:
        raise ValueError("ordinary home parent is unsafe")
    paths = ((home, stat.S_ISDIR, 0o750), (ssh_dir, stat.S_ISDIR, 0o700),
             (authorized, stat.S_ISREG, 0o600))
    existing = {}
    # Inspect every existing object before repairing any of them. Never follow links
    # or take over another ordinary user's files; no recursive ownership changes.
    for path, expected_type, _ in paths:
        try:
            attrs = sftp.lstat(path)
        except OSError as exc:
            if exc.errno != errno.ENOENT:
                raise
            attrs = None
        if attrs is not None and (not expected_type(attrs.st_mode) or attrs.st_uid not in (0, uid)):
            raise ValueError("authorization path identity mismatch")
        existing[path] = attrs
    for path, expected_type, mode in paths:
        attrs = existing[path]
        if path == home and attrs is not None:
            # Preserve a usable existing home mode while removing group/other write.
            mode = (stat.S_IMODE(attrs.st_mode) & 0o755) | 0o700
        if attrs is None:
            if expected_type is stat.S_ISDIR:
                sftp.mkdir(path, mode=mode)
            else:
                # Paramiko needs 'w' in addition to 'x' for a writable O_EXCL handle.
                with sftp.open(path, "wxb"):
                    pass
            attrs = sftp.lstat(path)
        if not expected_type(attrs.st_mode) or attrs.st_uid not in (0, uid):
            raise ValueError("authorization path identity mismatch")
        if stat.S_IMODE(attrs.st_mode) != mode:
            sftp.chmod(path, mode)
        if attrs.st_uid != uid or attrs.st_gid != gid:
            sftp.chown(path, uid, gid)
        attrs = sftp.lstat(path)
        if (not expected_type(attrs.st_mode) or attrs.st_uid != uid or attrs.st_gid != gid
                or stat.S_IMODE(attrs.st_mode) != mode):
            raise ValueError("authorization initialization readback mismatch")
    with sftp.open(authorized, "rb") as stream:
        before = stream.read(1024 * 1024 + 1)
    if len(before) > 1024 * 1024:
        raise ValueError("authorization file is oversized")
    if raw_line in before.splitlines():
        return "ALREADY_PRESENT"
    addition = (b"\n" if before and not before.endswith(b"\n") else b"") + raw_line + b"\n"
    with sftp.open(authorized, "ab") as stream:
        stream.write(addition)
        stream.flush()
    with sftp.open(authorized, "rb") as stream:
        after = stream.read(1024 * 1024 + len(addition) + 1)
    if after != before + addition:
        raise ValueError("authorization append readback mismatch")
    return "APPENDED"


def connect_ssh(
    host: str,
    user: str,
    known_hosts: Path | str,
    *,
    password: bytes,
    port: int = FIXED_PORT,
    timeout: float = 15.0,
):
    """Connect using an explicit host-key file before password authentication.

    The caller owns the returned client and must close it. Missing or unknown
    host keys fail closed; this function never enrolls or rewrites known_hosts.
    """
    if not isinstance(host, str) or not host or any(c.isspace() or ord(c) < 32 for c in host):
        raise ValueError("SSH host is invalid")
    if not isinstance(user, str) or not user or any(c.isspace() or ord(c) < 32 for c in user):
        raise ValueError("SSH user is invalid")
    if type(port) is not int or not 1 <= port <= 65535:
        raise ValueError("SSH port is invalid")
    if not isinstance(password, bytes) or not password or len(password) > MAX_SECRET:
        raise ValueError("SSH password is empty or oversized")
    if not isinstance(timeout, (int, float)) or timeout <= 0 or timeout > 60:
        raise ValueError("SSH timeout is invalid")
    known_file = Path(known_hosts).expanduser().resolve()
    if not known_file.is_file():
        raise FileNotFoundError("known_hosts file is missing")
    try:
        password_text = password.decode("utf-8")
    except UnicodeDecodeError as exc:
        raise ValueError("SSH password is not valid UTF-8") from exc

    import paramiko

    client = paramiko.SSHClient()
    client.load_host_keys(str(known_file))
    client.set_missing_host_key_policy(paramiko.RejectPolicy())
    try:
        client.connect(
            hostname=host,
            port=port,
            username=user,
            password=password_text,
            pkey=None,
            allow_agent=False,
            look_for_keys=False,
            timeout=timeout,
            auth_timeout=timeout,
            banner_timeout=timeout,
        )
    except Exception:
        client.close()
        raise
    return client


def main() -> int:
    raw = bytearray()
    root_password = bytearray()
    model_token = bytearray()
    metadata: dict[str, Any] | None = None
    try:
        import sys
        raw.extend(sys.stdin.buffer.read(MAX_FRAME + 1))
        if len(raw) > MAX_FRAME:
            raise ValueError("worker provisioning frame is oversized")
        metadata, password, token = decode_frame(bytes(raw))
        root_password.extend(password)
        model_token.extend(token)
        validate_request(metadata, bytes(root_password), bytes(model_token), require_output_dir=True)
        # output_dir is Manager-owned IPC context. Helper has no fallback to its
        # installation directory and must block before any network operation.
        output_dir = _validate_output_dir(metadata["output_dir"])
        install_root = Path(__file__).resolve().parent
        prerequisites = load_prerequisites_descriptor(_descriptor_path(install_root))
        result, private_key = provision_worker(
            metadata, bytes(root_password), bytes(model_token), prerequisites, install_root, output_dir,
        )
        sys.stdout.buffer.write(encode_result_frame(result, private_key))
        sys.stdout.buffer.flush()
        return 0
    except LocalPrerequisiteError as exc:
        try:
            facts = {}
            if metadata is not None and _safe_attempt_id(metadata.get("attempt_id")):
                facts["attempt_id"] = metadata["attempt_id"]
            result = redacted_result(
                "BLOCKED_OR_FAILED",
                failed_stage="local_prerequisites",
                error_type=exc.error_type,
                next_step=exc.next_step,
                root_password=bytes(root_password),
                model_token=bytes(model_token),
                **facts,
            )
            sys.stdout.buffer.write(encode_result_frame(result))
            sys.stdout.buffer.flush()
        except Exception:
            pass
        return 2
    except ProvisionStageError as exc:
        try:
            facts = {
                key: value for key, value in exc.diagnostics.items()
                if key in {"remote_root", "remote_side_effects", "reason_code"}
            }
            if metadata is not None and _safe_attempt_id(metadata.get("attempt_id")):
                facts["attempt_id"] = metadata["attempt_id"]
            result = redacted_result(
                "BLOCKED_OR_FAILED", failed_stage="orchestration",
                error_type=exc.error_type, next_step=exc.next_step,
                root_password=bytes(root_password), model_token=bytes(model_token),
                **facts,
            )
            sys.stdout.buffer.write(encode_result_frame(result))
            sys.stdout.buffer.flush()
        except Exception:
            pass
        return 2
    except Exception as exc:
        try:
            result = redacted_result(
                "BLOCKED_OR_FAILED",
                failed_stage="input",
                error_type=type(exc).__name__,
                next_step="检查六项输入后重试",
                root_password=bytes(root_password),
                model_token=bytes(model_token),
            )
            sys.stdout.buffer.write(encode_result_frame(result))
            sys.stdout.buffer.flush()
        except Exception:
            pass
        return 2
    finally:
        clear_buffers(raw, root_password, model_token)


if __name__ == "__main__":
    raise SystemExit(main())
