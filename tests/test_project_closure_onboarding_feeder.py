"""Tests for the interactive AW-WORKER-ONBOARDING/1 console feeder."""
from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import struct


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts" / "project-closure" / "feed_onboarding_secrets.py"
CLOSURE = ROOT / "tools" / "live_project_closure.py"


def load_script():
    spec = importlib.util.spec_from_file_location("feed_onboarding_secrets", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_closure():
    spec = importlib.util.spec_from_file_location("live_project_closure", CLOSURE)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def decode_frame(frame: bytes) -> tuple[bytes, bytes]:
    feeder = load_script()
    assert frame.startswith(feeder.MAGIC)
    offset = len(feeder.MAGIC)
    first_len = struct.unpack(">I", frame[offset:offset + 4])[0]
    offset += 4
    first = frame[offset:offset + first_len]
    offset += first_len
    second_len = struct.unpack(">I", frame[offset:offset + 4])[0]
    offset += 4
    second = frame[offset:offset + second_len]
    assert offset + second_len == len(frame)
    return first, second


def test_frame_round_trips_both_secret_fields_exactly():
    feeder = load_script()

    frame = feeder.encode_onboarding_frame(b"root-password", b"model-token")

    assert decode_frame(frame) == (b"root-password", b"model-token")


def test_frame_is_accepted_by_the_closure_runner_lease():
    feeder = load_script()
    closure = load_closure()

    frame = feeder.encode_onboarding_frame(b"root-password", b"model-token")

    with closure.stdin_secrets_lease(io.BytesIO(frame)) as (root_password, model_token):
        assert root_password == b"root-password"
        assert model_token == b"model-token"


def test_frame_rejects_empty_or_oversized_fields():
    feeder = load_script()

    for root_password, model_token in (
        (b"", b"t"), (b"r", b""), (b"r" * 16385, b"t"), (b"r", b"t" * 16385),
    ):
        try:
            feeder.encode_onboarding_frame(root_password, model_token)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid onboarding secret field was accepted")
