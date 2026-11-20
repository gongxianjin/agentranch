"""Behavior tests for the native Manager clipboard shortcut boundary.

Only the Win32 input boundary is intercepted.  These tests never read or write
clipboard contents and never send input to a real window.
"""
from __future__ import annotations

import importlib.util
from pathlib import Path
from unittest import mock

import pytest


ROOT = Path(__file__).resolve().parents[1]
DRIVER_PATH = ROOT / ".test-scripts" / "native-manager" / "config_only_direct_capture.py"


def load_native_driver():
    spec = importlib.util.spec_from_file_location("native_manager_clipboard_driver", DRIVER_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class HotkeyBoundary:
    def __init__(self):
        self.foreground_guards = []
        self.sent = []

    def assert_foreground(self, hwnd, pid):
        self.foreground_guards.append((hwnd, pid))

    def send_inputs(self, items):
        self.sent.append([
            (item.type, item.ki.wVk, item.ki.wScan, item.ki.dwFlags)
            for item in items
        ])


def test_ctrl_c_and_ctrl_v_emit_only_the_guarded_virtual_key_sequence():
    """The native adapter must produce real Ctrl+C/V primitives, not clipboard payloads."""
    driver = load_native_driver()
    boundary = HotkeyBoundary()

    with mock.patch.object(driver, "assert_foreground", side_effect=boundary.assert_foreground), \
         mock.patch.object(driver, "send_inputs", side_effect=boundary.send_inputs):
        driver.hotkey_ctrl_c(101, 202)
        driver.hotkey_ctrl_v(303, 404)

    assert boundary.foreground_guards == [(101, 202), (303, 404)]
    assert boundary.sent == [
        [
            (1, 0x11, 0, 0),
            (1, 0x43, 0, 0),
            (1, 0x43, 0, driver.KEYEVENTF_KEYUP),
            (1, 0x11, 0, driver.KEYEVENTF_KEYUP),
        ],
        [
            (1, 0x11, 0, 0),
            (1, 0x56, 0, 0),
            (1, 0x56, 0, driver.KEYEVENTF_KEYUP),
            (1, 0x11, 0, driver.KEYEVENTF_KEYUP),
        ],
    ]
    assert all(item[2] == 0 for sequence in boundary.sent for item in sequence), \
        "shortcut primitives must not carry clipboard text through Unicode scan codes"


def test_eframe_clipboard_feature_is_enabled_for_windows_system_clipboard():
    """The native eframe window must bridge Ctrl+V to the Windows clipboard."""
    import tomllib

    cargo = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    features = cargo["dependencies"]["eframe"]["features"]
    assert "clipboard" in features


def test_find_window_ignores_tiny_helper_windows_and_selects_largest_visible_window():
    """A same-process 4x4 helper HWND must never receive GUI input or screenshots."""
    driver = load_native_driver()
    windows = [
        {"hwnd": 11, "title": "", "visible": True, "rect": [0, 0, 4, 4]},
        {"hwnd": 22, "title": "agentranch ����̨", "visible": True, "rect": [25, 25, 1140, 862]},
    ]
    assert driver.select_manager_window(windows) == 22


def test_helper_only_window_batch_is_not_a_terminal_manager_lookup_failure():
    """A transient helper-only enumeration must let find_window keep waiting."""
    driver = load_native_driver()
    assert driver.select_manager_window([
        {"hwnd": 11, "title": "", "visible": True, "rect": [0, 0, 4, 4]},
    ]) is None


def test_find_window_waits_for_main_window_after_helper_only_snapshot():
    """find_window must not fail before a later enumeration exposes the main HWND."""
    driver = load_native_driver()
    pid = 202
    snapshots = [
        [11],
        [11, 22],
    ]
    rectangles = {
        11: [0, 0, 4, 4],
        22: [25, 25, 1140, 862],
    }
    calls = []

    def enum_windows(callback, _lparam):
        snapshot = snapshots[min(len(calls), len(snapshots) - 1)]
        calls.append(snapshot)
        for hwnd in snapshot:
            callback(hwnd, 0)
        return True

    def get_window_rect(hwnd, rect):
        left, top, right, bottom = rectangles[hwnd]
        target = rect._obj
        target.left, target.top, target.right, target.bottom = left, top, right, bottom
        return True

    with mock.patch.object(driver.user, "EnumWindows", side_effect=enum_windows), \
         mock.patch.object(driver.user, "IsWindowVisible", return_value=True), \
         mock.patch.object(driver.user, "GetWindowRect", side_effect=get_window_rect), \
         mock.patch.object(driver, "owned_pid", return_value=pid), \
         mock.patch.object(driver, "window_title", return_value=""), \
         mock.patch.object(driver.time, "sleep"):
        assert driver.find_window(pid, timeout=1) == 22

    assert len(calls) == 2


def test_assert_foreground_attaches_to_existing_foreground_thread_before_activation():
    """Foreground activation must work when Windows rejects a direct SetForegroundWindow call."""
    driver = load_native_driver()
    foreground_hwnd = 33
    target_hwnd = 22
    foreground_thread = 55
    current_thread = 44
    with mock.patch.object(driver, "owned_pid", return_value=202), \
         mock.patch.object(driver.kernel, "GetCurrentThreadId", return_value=current_thread), \
         mock.patch.object(driver.user, "GetForegroundWindow", side_effect=[foreground_hwnd, target_hwnd]), \
         mock.patch.object(driver.user, "GetWindowThreadProcessId", return_value=foreground_thread), \
         mock.patch.object(driver.user, "AttachThreadInput", return_value=1) as attach, \
         mock.patch.object(driver.user, "BringWindowToTop", return_value=1) as bring, \
         mock.patch.object(driver.user, "SetForegroundWindow", return_value=1) as activate, \
         mock.patch.object(driver.time, "sleep"):
        driver.assert_foreground(target_hwnd, 202)

    attach.assert_has_calls([mock.call(current_thread, foreground_thread, True), mock.call(current_thread, foreground_thread, False)])
    bring.assert_called_once_with(target_hwnd)
    activate.assert_called_once_with(target_hwnd)


def test_ctrl_c_and_ctrl_v_fail_closed_when_the_owned_window_guard_rejects_input():
    """A failed HWND/PID/foreground check must prevent every SendInput call."""
    driver = load_native_driver()
    sent = []

    def reject(hwnd, pid):
        raise RuntimeError(f"guard rejected {hwnd}:{pid}")

    with mock.patch.object(driver, "assert_foreground", side_effect=reject), \
         mock.patch.object(driver, "send_inputs", side_effect=lambda items: sent.append(items)):
        with pytest.raises(RuntimeError, match="guard rejected 101:202"):
            driver.hotkey_ctrl_c(101, 202)
        with pytest.raises(RuntimeError, match="guard rejected 303:404"):
            driver.hotkey_ctrl_v(303, 404)

    assert sent == []
