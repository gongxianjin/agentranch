"""Tests for the in-memory project-closure secret frame feeder."""
from __future__ import annotations

import importlib.util
from pathlib import Path
import struct


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / ".test-scripts" / "project-closure" / "feed_secrets.py"


def load_script():
    spec = importlib.util.spec_from_file_location("project_closure_secret_feeder", SCRIPT)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def test_encode_frame_keeps_key_and_token_in_bounded_binary_fields():
    feeder = load_script()
    key = b"private-key-bytes"
    token = b"model-token-bytes"

    frame = feeder.encode_frame(key, token)

    assert frame.startswith(feeder.MAGIC)
    offset = len(feeder.MAGIC)
    key_len = struct.unpack(">I", frame[offset:offset + 4])[0]
    offset += 4
    assert frame[offset:offset + key_len] == key
    offset += key_len
    token_len = struct.unpack(">I", frame[offset:offset + 4])[0]
    offset += 4
    assert frame[offset:offset + token_len] == token
    assert offset + token_len == len(frame)


def test_encode_frame_rejects_empty_or_oversized_fields():
    feeder = load_script()

    for key, token in ((b"", b"t"), (b"k", b""), (b"k" * 65537, b"t"), (b"k", b"t" * 16385)):
        try:
            feeder.encode_frame(key, token)
        except ValueError:
            pass
        else:
            raise AssertionError("invalid secret frame field was accepted")
