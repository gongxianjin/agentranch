"""Real Manager context/evidence; only Win32 and child process boundaries simulated."""
import importlib.util
import json
from pathlib import Path
import subprocess
from types import SimpleNamespace

import pytest

ROOT = Path(__file__).resolve().parents[1]


class Child:
    pid = 4242

    def __init__(self, *, exited=None, wait_code=0, timeout=False):
        self.exited = exited
        self.wait_code = wait_code
        self.timeout = timeout
        self.forces = []
        self.waits = []

    def poll(self):
        return self.exited

    def wait(self, timeout):
        self.waits.append(timeout)
        if self.timeout and not self.forces:
            raise subprocess.TimeoutExpired("synthetic-manager", timeout)
        self.exited = -15 if self.forces else self.wait_code
        return self.exited

    def terminate(self):
        self.forces.append("terminate")

    def kill(self):
        self.forces.append("kill")


def exercise(monkeypatch, tmp_path, child, *, owner=4242, post=1, missing=False):
    spec = importlib.util.spec_from_file_location("shutdown_driver", ROOT / "tools/live_project_closure.py")
    driver = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(driver)
    messages = []
    monkeypatch.setattr(driver, "os", SimpleNamespace(name="nt", environ=driver.os.environ))

    def post_message(*args):
        messages.append(args)
        return post

    ui = SimpleNamespace(
        ctypes=SimpleNamespace(c_void_p=lambda n: n),
        user=SimpleNamespace(SetProcessDpiAwarenessContext=lambda _: None, PostMessageW=post_message),
        WM_CLOSE=16, owned_pid=lambda _: owner, capture=lambda *args: {"owned": True},
    )
    monkeypatch.setattr(driver, "load_module", lambda *args: ui)
    monkeypatch.setattr(driver.subprocess, "Popen", lambda *args, **kw: child)
    monkeypatch.setattr(driver, "find_manager_window", lambda *args: None if missing else 1234)
    caught = None
    try:
        with driver.manager_session((b"synthetic-root", b"synthetic-token"),
                {"package": tmp_path, "document": tmp_path / "design.md"}, tmp_path):
            pass
    except driver.ClosureCheckError as exc:
        caught = exc
    record = json.loads((tmp_path / "manager-exit.json").read_text())
    return record, caught, messages


def test_owned_normal_close_records_direct_wait_zero(monkeypatch, tmp_path):
    child = Child()
    record, error, messages = exercise(monkeypatch, tmp_path, child)
    assert error is None
    assert messages == [(1234, 16, 0, 0)]
    assert child.forces == []
    assert child.waits == [15]
    assert record["exit_code"] == 0
    assert record["windows_manager_wait_completed"] is True
    assert record["forced_owned_child_terminate"] is False


def test_already_exited_child_never_receives_close(monkeypatch, tmp_path):
    child = Child(exited=0)
    record, error, messages = exercise(monkeypatch, tmp_path, child)
    assert error is None
    assert messages == []
    assert child.waits == [15]
    assert record["exit_code"] == 0
    assert record["windows_manager_wait_completed"] is True


@pytest.mark.parametrize("owner,post,missing,expected_messages", [
    (4242, 1, False, [(1234, 16, 0, 0)]),
    (9999, 1, False, []),
    (4242, 0, False, [(1234, 16, 0, 0)]),
    (4242, 1, True, []),
])
def test_timeout_never_forces_or_invents_exit(monkeypatch, tmp_path, owner, post, missing, expected_messages):
    child = Child(timeout=True)
    record, error, messages = exercise(monkeypatch, tmp_path, child, owner=owner, post=post, missing=missing)
    assert child.forces == []
    assert child.waits == [15]
    assert messages == expected_messages
    assert error is not None
    assert str(error) == "manager_shutdown_timeout"
    assert record["exit_code"] is None
    assert record["windows_manager_wait_completed"] is False
    assert record["forced_owned_child_terminate"] is False
    assert record["process_disposition"] == "LEFT_OWNED_PROCESS_UNTERMINATED"


def test_failed_close_message_is_not_claimed_delivered(monkeypatch, tmp_path):
    record, error, _ = exercise(monkeypatch, tmp_path, Child(), post=0)
    # The child may exit independently; observed wait0 remains valid, delivery does not.
    assert error is None
    assert record.get("close_request") == "POST_FAILED"
    assert record["exit_code"] == 0
    assert record["windows_manager_wait_completed"] is True


def test_nonzero_exit_remains_failure_without_force(monkeypatch, tmp_path):
    child = Child(wait_code=7)
    record, error, _ = exercise(monkeypatch, tmp_path, child)
    assert str(error) == "manager_shutdown_failed"
    assert child.forces == []
    assert record["exit_code"] == 7
    assert record["windows_manager_wait_completed"] is True
