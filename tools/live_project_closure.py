"""Run one project-work-v1 dispatch after six-field Worker onboarding.

The novice flow supplies Worker IP, root account/password, HTTPS model origin,
API key, and model. Provisioning is performed by the Manager and its bundled
Helper; this harness drives the owned GUI and verifies the fresh Store.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from live_manager_closure import (ClosureCheckError, load_module,
                                  read_channel, require, write_json_new,
                                  _require_owned_window_ready, find_manager_window)

ALT_VK = 0x12  # VK_MENU
KEYEVENTF_KEYUP = 0x0002


def wait_for_owned_foreground_robust(ui, hwnd, pid, timeout=15, poll_interval=0.1):
    """Foreground guard that survives an idle review gap.

    Same ownership invariants as live_manager_closure's guard (owned pid,
    visible, >=720x540, exact title, and foreground == hwnd before any
    input is sent). The only addition: when the OS denies the raise because
    this driver process lost foreground-eligibility during the gap (the
    2026-11-12 13:32 HUMAN_FOREGROUND_REQUIRED incident), restore
    eligibility the documented way — one synthetic ALT tap via SendInput
    makes the calling process the last input recipient — then re-issue
    SetForegroundWindow. Every input still requires GetForegroundWindow()
    == hwnd afterwards; nothing is bypassed.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        _require_owned_window_ready(ui, hwnd, pid)
        if int(ui.user.GetForegroundWindow() or 0) == int(hwnd):
            return True
        extra = ui.ctypes.pointer(ui.ctypes.c_ulong(0))
        try:
            ui.send_inputs([
                ui.INPUT(type=1, ki=ui.KEYBDINPUT(ALT_VK, 0, 0, 0, extra)),
                ui.INPUT(type=1, ki=ui.KEYBDINPUT(ALT_VK, 0, KEYEVENTF_KEYUP, 0, extra)),
            ])
        except AttributeError:
            pass  # stub without INPUT structures: raise-only fallback below
        ui.user.SetForegroundWindow(hwnd)
        time.sleep(poll_interval)
    raise ClosureCheckError("HUMAN_FOREGROUND_REQUIRED")


ONBOARDING_MAGIC = b"AW-WORKER-ONBOARDING/1\n"
SECRET_FIELD_MAX = 16 * 1024


def discover_generated_gateway(state_root, before_outputs=()):
    """Return the sole gateway generated during this onboarding run.

    Only the Manager-owned provisioning subtree is searched.  Existing output
    directories are supplied by the caller before the Manager starts, so an
    old gateway can never be selected as the new connection.
    """
    root = Path(state_root).resolve()
    provisioning = root / "provisioning"
    if not provisioning.is_dir():
        raise ClosureCheckError("generated_gateway_missing")
    before = {Path(path).resolve() for path in before_outputs}
    candidates = []
    for gateway in provisioning.glob("*/gateway.json"):
        if gateway.is_symlink() or not gateway.is_file():
            continue
        output = gateway.parent.resolve()
        if output in before:
            continue
        candidates.append(gateway.resolve())
    if not candidates:
        raise ClosureCheckError("generated_gateway_missing")
    if len(candidates) != 1:
        raise ClosureCheckError("generated_gateway_ambiguous")
    return candidates[0]


ONBOARDING_FIELDS = (
    "host", "root-user", "root-password", "model-base-url", "model-token", "model",
)


def _read_onboarding_field(field, session):
    values = {
        "host": session["host"],
        "root-user": session["root_user"],
        "root-password": session["root_password"],
        "model-base-url": session["model_base_url"],
        "model-token": session["model_token"],
        "model": session["model"],
    }
    require(field in values, "unknown_onboarding_field")
    value = values[field]
    if isinstance(value, bytes):
        return value.decode("utf-8")
    return value


def prepare_onboarding(args, secrets, evidence):
    """Validate non-secret run inputs without requiring a Gateway path."""
    root_password, model_token = secrets
    require(0 < len(root_password) <= SECRET_FIELD_MAX, "root_password_bound")
    require(0 < len(model_token) <= SECRET_FIELD_MAX, "model_token_bound")
    require(args.host.strip() and args.root_user.strip() and args.model.strip(),
            "onboarding_identity_required")
    require(args.model_base_url.startswith("https://") and
            "/" not in args.model_base_url[8:] and
            "?" not in args.model_base_url and "#" not in args.model_base_url,
            "onboarding_https_origin_required")
    package = Path(args.package).expanduser().resolve()
    require(package.is_dir(), "package_directory_missing")
    manifest = manifest_json(args.package_manifest)
    if "package_dir" in manifest:
        require(Path(manifest["package_dir"]).resolve() == package,
                "package_manifest_path_mismatch")
    document = Path(args.document).expanduser().resolve()
    require(document.is_file(), "design_document_missing")
    document_facts = file_identity(document)
    write_json_new(evidence / "onboarding-preflight.json", {
        "schema": "worker-onboarding-preflight/1",
        "host": args.host.strip(),
        "root_user": args.root_user.strip(),
        "model_base_url": args.model_base_url,
        "model": args.model,
        "document": document_facts,
        "secret_fields": {"root_password": "memory_only", "model_token": "memory_only"},
    })
    state_root = evidence / "manager-state"
    before = set()
    provisioning = state_root / "provisioning"
    if provisioning.is_dir():
        before = {path.resolve() for path in provisioning.iterdir() if path.is_dir()}
    return {
        "mode": "onboarding",
        "host": args.host.strip(),
        "root_user": args.root_user.strip(),
        "model_base_url": args.model_base_url,
        "model": args.model,
        "package": package,
        "manifest": manifest,
        "document": document,
        "document_sha256": document_facts["sha256"],
        "evidence": evidence,
        "state_root": state_root,
        "before_outputs": before,
        "gateway": None,
        "database": None,
    }


@contextlib.contextmanager
def stdin_secrets_lease(stream):
    """Read root password + model token once, then zero the frame."""
    raw = bytearray(stream.read(2 * SECRET_FIELD_MAX + len(ONBOARDING_MAGIC) + 8 + 1))
    try:
        require(raw.startswith(ONBOARDING_MAGIC), "framed_onboarding_secrets_required")
        offset = len(ONBOARDING_MAGIC)

        def read_field(offset):
            require(offset + 4 <= len(raw), "truncated_secret_field")
            length = int.from_bytes(raw[offset:offset + 4], "big")
            require(0 < length <= SECRET_FIELD_MAX, "secret_field_bound")
            start, end = offset + 4, offset + 4 + length
            field = bytes(raw[start:end])
            require(len(field) == length, "truncated_secret_field")
            return field, end

        root_password, offset = read_field(offset)
        model_token, offset = read_field(offset)
        require(offset == len(raw), "trailing_bytes_in_secrets_frame")
        yield root_password, model_token
    finally:
        raw[:] = b"\x00" * len(raw)


def bounded_json(path):
    with Path(path).open("rb") as stream:
        raw = stream.read(65537)
    require(len(raw) <= 65536, "configuration_oversized")
    return json.loads(raw)


def manifest_json(path):
    """Package manifests list every bundled file (thousands of entries); the
    driver re-hashes each pinned file itself, so a wider read bound is safe."""
    with Path(path).open("rb") as stream:
        raw = stream.read(1 << 24)
    require(len(raw) <= (1 << 24) - 1, "package_manifest_oversized")
    return json.loads(raw)


def read_remote_endpoint(sftp, endpoint_path):
    """Read the endpoint config while hiding remote path details from evidence."""
    try:
        with sftp.open(endpoint_path, "rb") as stream:
            raw = stream.read(65537)
    except FileNotFoundError as exc:
        raise ClosureCheckError("remote_model_endpoint_missing") from exc
    require(len(raw) <= 65536, "remote_model_endpoint_oversized")
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise ClosureCheckError("remote_model_endpoint_invalid_json") from exc


def file_identity(path):
    with Path(path).open("rb") as stream:
        return {"bytes": Path(path).stat().st_size,
                "sha256": hashlib.file_digest(stream, "sha256").hexdigest()}


def _refresh_generated_gateway(session):
    """Bind dispatch inspection to the sole gateway made in this run."""
    if session.get("database") is not None:
        return True
    path = discover_generated_gateway(session["state_root"], session["before_outputs"])
    gateway = bounded_json(path)
    output_root = path.parent.resolve()
    controller_path = Path(gateway["controller_config"]).resolve()
    require(controller_path.parent == output_root, "generated_controller_path_escape")
    controller = bounded_json(controller_path)
    state = Path(controller["state_dir"]).resolve()
    require(state.parent == output_root and state.name == "controller-state",
            "generated_state_path_escape")
    session.update(gateway_path=path, gateway=gateway, database=state / "controller.sqlite")
    return True


def require_current_preflight_admission(session):
    """Require the current run's preflight ledger before dispatch input."""
    state_root = Path(session["state_root"]).resolve()
    ledger_path = state_root / "provisioning-ledger.json"
    if ledger_path.is_symlink() or not ledger_path.is_file():
        raise ClosureCheckError("current_preflight_missing")
    try:
        ledger = bounded_json(ledger_path)
    except (OSError, json.JSONDecodeError, TypeError, ValueError) as exc:
        raise ClosureCheckError("current_preflight_invalid") from exc
    if not isinstance(ledger, dict) or ledger.get("schema") != "worker-provision-ledger/1":
        raise ClosureCheckError("current_preflight_invalid")
    if ledger.get("state") != "preflight":
        raise ClosureCheckError("current_preflight_missing")
    current_gateway = Path(session["gateway_path"]).resolve()
    current_output = current_gateway.parent
    try:
        selected_output = Path(ledger["output_dir"]).resolve()
        selected_gateway = Path(ledger["gateway_path"]).resolve()
    except (KeyError, TypeError, ValueError, OSError) as exc:
        raise ClosureCheckError("current_preflight_invalid") from exc
    if selected_output != current_output or selected_gateway != current_gateway:
        raise ClosureCheckError("current_preflight_mismatch")
    if selected_gateway.name != "gateway.json" or selected_output.parent != state_root / "provisioning":
        raise ClosureCheckError("current_preflight_invalid")
    return ledger


def close_manager_owned(ui, process, hwnd, evidence, timeout=15):
    """Request owned WM_CLOSE, then observe a direct wait; never force exit."""
    close_request = "ALREADY_EXITED"
    if process.poll() is None:
        close_request = "NO_OWNED_WINDOW"
        if hwnd is not None and ui.owned_pid(hwnd) == process.pid:
            close_request = (
                "POSTED" if ui.user.PostMessageW(hwnd, ui.WM_CLOSE, 0, 0)
                else "POST_FAILED"
            )
    code, completed = None, False
    try:
        code = process.wait(timeout=timeout)
        completed = True
    except subprocess.TimeoutExpired:
        pass
    write_json_new(evidence / "manager-exit.json", {
        "pid": process.pid,
        "exit_code": code,
        "close_request": close_request,
        "forced_owned_child_terminate": False,
        "windows_manager_wait_completed": completed,
        "process_disposition": "EXIT_CONFIRMED" if completed else "LEFT_OWNED_PROCESS_UNTERMINATED",
        "linux_owner_wait": "NOT_INFERRED_FROM_WINDOWS_PROCESS_EXIT",
    })
    require(completed, "manager_shutdown_timeout")
    require(code == 0, "manager_shutdown_failed")


@contextlib.contextmanager
def manager_session(secrets, prepared, evidence):
    """Open the owned Manager with no credential-bearing argv/env/stdin."""
    require(os.name == "nt", "windows_manager_required")
    root_password, model_token = secrets
    ui_path = Path(__file__).resolve().parents[1] / ".test-scripts/native-manager/config_only_direct_capture.py"
    ui = load_module(ui_path, "project_closure_owned_window")
    ui.user.SetProcessDpiAwarenessContext(ui.ctypes.c_void_p(-4))
    package = prepared["package"]
    system_root = os.environ.get("SystemRoot", "")
    env = {"SystemRoot": system_root, "PATH": str(package)}
    process, hwnd = None, None
    state_root = evidence / "manager-state"
    with (evidence / "manager.stdout.log").open("xb") as output, (evidence / "manager.stderr.log").open("xb") as error:
        try:
            process = subprocess.Popen([str(package / "agentranch.exe"), "--real",
                "--state-dir", str(state_root)],
                cwd=str(package), env=env, stdin=subprocess.DEVNULL, stdout=output, stderr=error)
            hwnd = find_manager_window(ui, process.pid)
            initial = ui.capture(hwnd, process.pid, evidence / "window-initial.png")
            write_json_new(evidence / "manager-window.json", {
                "pid": process.pid, "hwnd": hwnd, "initial": initial,
                "visual_review": "REQUIRED_BEFORE_COORDINATE_ACTIONS",
                "document_path": str(prepared["document"]),
                "control": "Type six fields from memory, inspect screenshots, provision once, then dispatch once.",
            })
            (evidence / "controls").mkdir(exist_ok=False)
            yield {**prepared, "process": process, "hwnd": hwnd, "ui": ui,
                   "root_password": root_password, "model_token": model_token,
                   "state_root": state_root, "gateway": None, "database": None}
        finally:
            if process is not None:
                close_manager_owned(ui, process, hwnd, evidence)


def drive_manager(session, control_timeout=900):
    """Sequential queue: fill onboarding, provision, then dispatch once."""
    evidence, ui = session["evidence"], session["ui"]
    process, hwnd = session["process"], session["hwnd"]
    deadline = time.monotonic() + control_timeout
    handled = set()
    while time.monotonic() < deadline:
        require(process.poll() is None, "manager_exited_before_dispatch")
        for path in sorted((evidence / "controls").glob("command-*.json")):
            if path.name in handled:
                continue
            command = bounded_json(path)
            action = command["action"]
            require(action in ("click", "type-field", "provision", "type-document", "capture", "dispatch", "abort"),
                    "unsupported_gui_action")
            require(action != "abort", "operator_aborted")
            if action in ("click", "dispatch", "provision"):
                x, y = command["x"], command["y"]
                rect = ui.RECT()
                require(ui.user.GetWindowRect(hwnd, ui.ctypes.byref(rect)), "window_rect_unavailable")
                require(type(x) is int and type(y) is int and 0 <= x < rect.right - rect.left
                        and 0 <= y < rect.bottom - rect.top, "click_outside_owned_window")
                if action == "dispatch":
                    _refresh_generated_gateway(session)
                    require_current_preflight_admission(session)
                    require(session["database"].is_file(), "provision_worker_before_dispatch")
                    with contextlib.closing(sqlite3.connect(session["database"].as_uri() + "?mode=ro", uri=True)) as db:
                        require(db.execute("SELECT count(*) FROM runs").fetchone()[0] == 0,
                                "fresh_store_required_before_dispatch")
                wait_for_owned_foreground_robust(ui, hwnd, process.pid)
                ui.click(hwnd, process.pid, x, y)
            elif action == "provision":
                wait_for_owned_foreground_robust(ui, hwnd, process.pid)
                ui.click(hwnd, process.pid, x, y)
            elif action == "type-field":
                field = command.get("field")
                require(field in ONBOARDING_FIELDS, "unsupported_onboarding_field")
                wait_for_owned_foreground_robust(ui, hwnd, process.pid)
                ui.hotkey_ctrl_a(hwnd, process.pid)
                ui.unicode_type(hwnd, process.pid, _read_onboarding_field(field, session))
            elif action == "type-document":
                wait_for_owned_foreground_robust(ui, hwnd, process.pid)
                ui.hotkey_ctrl_a(hwnd, process.pid)
                ui.unicode_type(hwnd, process.pid, str(session["document"]))
            image = ui.capture(hwnd, process.pid, evidence / (path.stem + ".png"))
            write_json_new(evidence / (path.stem + "-result.json"), {"action": action, "screenshot": image})
            handled.add(path.name)
            if action == "dispatch":
                return
        time.sleep(0.2)
    raise ClosureCheckError("gui_control_deadline_exceeded")


def inspect_manager_diag(state_root, run_id):
    """Gate: the append_diag fix must show all three events for this run.

    Real-mode managers never created --state-dir at startup, so the first
    diag lines (gateway_configured, dispatch_result) were silently dropped
    before the fix (sessions -41/-43 logged only artifact_materialized;
    session -44 with the fix logged all three). This check turns that
    regression class into a harness failure instead of silent line loss.

    Line format written by append_diag: "{ts}\\t{event}\\t{detail}\\n".
    """
    facts = {"status": "NOT_CONFIRMED", "reason": "manager_diag_missing"}
    path = Path(state_root) / "manager-diag.log"
    if not path.is_file():
        return facts
    events = set()
    try:
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            parts = line.split("\t")
            if len(parts) != 3:
                continue
            _ts, event, detail = parts
            if event == "gateway_configured":
                events.add("gateway_configured")
            elif event in ("dispatch_result", "artifact_materialized"):
                if f"run_id={run_id}" in detail:
                    events.add(event)
            elif event == "artifact_error":
                if f"run_id={run_id}" in detail:
                    facts["reason"] = f"manager_diag_artifact_error:{detail}"
    except OSError as exc:
        facts["reason"] = f"manager_diag_unreadable:{type(exc).__name__}"
        return facts
    for required in ("gateway_configured", "dispatch_result", "artifact_materialized"):
        if required not in events:
            facts["reason"] = f"manager_diag_event_missing:{required}"
            return facts
    facts.pop("reason", None)
    facts.update(status="PASS_MANAGER_DIAG_SURVIVAL", events=sorted(events))
    return facts


def scan_state_for_token(root, token):
    """No token bytes in any file under the manager state directory."""
    hits = []
    for path in Path(root).rglob("*"):
        if path.is_file() and not path.is_symlink():
            try:
                if token in path.read_bytes():
                    hits.append(str(path))
            except OSError:
                hits.append(f"unreadable:{path}")
    return hits


def inspect_project_store(database, deployment, worker_id, artifact_sha256, document_sha256, state_root, token):
    """Check the project-work-v1 evidence chain and the artifact materialization."""
    database = Path(database).resolve()
    if not database.is_file():
        return {"status": "NOT_ADMITTED", "reason": "controller_database_missing"}
    facts = {"status": "NOT_ADMITTED", "used_slots": None}
    connection = sqlite3.connect(database.as_uri() + "?mode=ro", uri=True, timeout=3)
    connection.row_factory = sqlite3.Row
    try:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        identity = connection.execute("SELECT value FROM metadata WHERE key='deployment'").fetchone()
        require(identity is not None and identity[0] == deployment, "deployment_mismatch")
        facts["used_slots"] = connection.execute(
            "SELECT count(*) FROM assignments WHERE node=? AND released=0", (worker_id,)
        ).fetchone()[0]
        rows = connection.execute("SELECT id,body,status,attempt FROM runs").fetchall()
        require(len(rows) == 1, "expected_exactly_one_dispatched_run")
        row = rows[0]
        run = json.loads(row["body"])
        require(run["id"] == row["id"] and run["attempt"] == row["attempt"], "run_row_mismatch")
        facts.update(run_id=run["id"], template=run.get("template"), attempt=run["attempt"])
        require(run.get("template") == "project-work-v1", "project_run_template_mismatch")
        require(row["status"] == run["status"] == "done", "run_not_done")
        require(bool(run["session"]) and run["detail"].get("owner_admission") == "admitted", "owner_not_admitted")
        assignments = connection.execute("SELECT * FROM assignments").fetchall()
        require(len(assignments) == 1, "expected_exactly_one_assignment")
        assignment = assignments[0]
        require(assignment["run"] == run["id"] and assignment["attempt"] == run["attempt"]
                and assignment["node"] == worker_id and assignment["released"] == 1
                and facts["used_slots"] == 0, "assignment_not_released")
        report = run["detail"].get("report") or {}
        require(report.get("schema") == "native-project/1", "project_report_schema_mismatch")
        require(report.get("session") == run["session"], "project_report_session_mismatch")
        require(report.get("document_sha256") == document_sha256, "document_binding_mismatch")
        model = report.get("model", {})
        require(bool(model.get("model")) and bool(model.get("request_sha256"))
                and bool(model.get("response_sha256")) and int(model.get("elapsed_ms", -1)) >= 0
                and model.get("status") == 200, "model_call_evidence_missing")
        facts["model_evidence"] = {k: model[k] for k in ("model", "request_sha256", "response_sha256", "elapsed_ms", "status")}
        artifact = run["detail"].get("artifact") or report.get("artifact")
        require(isinstance(artifact, dict) and artifact.get("schema") == "project-artifact-package/1",
                "artifact_package_missing")
        require(artifact.get("document_sha256") == document_sha256, "package_document_binding_mismatch")
        files = artifact.get("files") or []
        require(len(files) >= 1, "artifact_files_empty")
        artifact_dir = Path(run["detail"].get("artifact_dir", "")) if run["detail"].get("artifact_dir") else None
        require(artifact_dir is not None and artifact_dir.is_dir(), "artifact_dir_missing")
        seen = set()
        for item in files:
            name = item.get("path", "")
            require(re.fullmatch(r"[A-Za-z0-9._/-]+", name) and ".." not in name and not name.startswith("/"),
                    "artifact_path_unsafe")
            require(name not in seen, "artifact_path_duplicate")
            seen.add(name)
            target = (artifact_dir / name).resolve()
            require(str(target).startswith(str(artifact_dir.resolve()) + os.sep), "artifact_escape")
            raw = target.read_bytes()
            require(len(raw) == item.get("size"), "artifact_size_mismatch")
            require(hashlib.sha256(raw).hexdigest() == item.get("sha256"), "artifact_digest_mismatch")
            require("content_hex" in item and bytes.fromhex(item["content_hex"]) == raw,
                    "artifact_content_hex_mismatch")
        require("index.html" in seen, "artifact_index_missing")
        html = (artifact_dir / "index.html").read_text(encoding="utf-8", errors="strict")
        # design.md defines the card selectors as runtime contract entries
        # ("结果：[data-results-list]、[data-card][data-id=...]"): data-card
        # is created by the renderer at runtime (article.dataset.card in the
        # referenced js), not present in the static index.html text. Mount
        # markers must sit in index.html itself; renderer markers may be
        # satisfied by any js file the page references. Static-text check
        # only (run ea3aed16, session -41): no browser is dragged into the
        # acceptance harness.
        js_text = "\n".join((artifact_dir / n).read_text(encoding="utf-8", errors="strict")
                            for n in seen if n.endswith(".js"))
        static_markers = ("data-search-input", "data-category-list", "data-results-list")
        renderer_markers = ("data-card",)
        for marker in static_markers:
            require(marker in html, f"stable_selector_missing:{marker}")
        for marker in renderer_markers:
            # The renderer may write the attribute either as an HTML string
            # ("data-card") or via the DOM API (dataset.card, camelCase) —
            # both produce the queryable [data-card] at runtime.
            parts = marker.split("-")[1:]
            dom_api = "dataset." + parts[0] + "".join(p.capitalize() for p in parts[1:])
            require(marker in html or marker in js_text or dom_api in js_text,
                    f"stable_selector_missing:{marker}")
        facts["artifact_dir"] = str(artifact_dir)
        facts["artifact_files"] = sorted(seen)
        # Token secrecy: no token bytes anywhere in the persisted manager state.
        hits = scan_state_for_token(state_root, token)
        require(not hits, "token_bytes_found_in_state:" + ",".join(hits[:5]))
        facts["token_absent_from_state"] = True
        facts.update(status="PASS_PROJECT_STORE_ADMISSION", artifact_sha256=artifact_sha256,
                     assignment_released=True, document_sha256=document_sha256)
    except ClosureCheckError as exc:
        facts["reason"] = str(exc)
    except (KeyError, TypeError, ValueError, sqlite3.Error, OSError) as exc:
        facts["reason"] = f"store_incomplete_or_malformed:{type(exc).__name__}"
    finally:
        connection.close()
    return facts


def wait_for_admission(session, timeout=480):
    deadline = time.monotonic() + timeout
    facts = {"status": "NOT_ADMITTED", "reason": "controller_database_missing"}
    while time.monotonic() < deadline:
        if session.get("database") is None:
            try:
                _refresh_generated_gateway(session)
            except ClosureCheckError as exc:
                if str(exc) != "generated_gateway_missing":
                    raise
        if session.get("database") is not None and session["database"].is_file():
            gateway = session["gateway"]
            facts = inspect_project_store(session["database"], gateway["deployment"], gateway["worker_id"],
                                          gateway["artifact_sha256"], session["document_sha256"],
                                          session["evidence"] / "manager-state", session["model_token"])
            if facts["status"] == "PASS_PROJECT_STORE_ADMISSION":
                break
        require(session["process"].poll() is None, "manager_exited_before_admission")
        time.sleep(1.0)
    session["ui"].capture(session["hwnd"], session["process"].pid, session["evidence"] / "window-terminal.png")
    write_json_new(session["evidence"] / "store-acceptance.json", facts)
    return facts


def run_project_closure(evidence, secrets, prepare, manager, dispatch, verify):
    evidence = Path(evidence)
    evidence.mkdir(parents=True, exist_ok=False)
    result = {"schema": "manager-project-closure/1", "status": "BLOCKED_OR_FAILED",
              "dispatch": "NOT_RUN", "store": "NOT_RUN", "credential_cleanup": "NOT_CONFIRMED"}
    stage = "credential_acquire"
    try:
        with secrets() as (key, token):
            stage = "remote_prepare"
            prepared = prepare((key, token), evidence) if prepare.__code__.co_argcount == 2 else prepare(args_ref[0], (key, token), evidence)
            stage = "manager_open"
            with manager((key, token), prepared, evidence) as session:
                stage = "manager_dispatch"
                dispatch(session)
                result["dispatch"] = "GUI_INPUT_SENT"
                stage = "store_acceptance"
                result["store"] = verify(session)
                facts = result["store"]
                require(isinstance(facts, dict)
                        and facts.get("status") == "PASS_PROJECT_STORE_ADMISSION"
                        and bool(facts.get("run_id"))
                        and facts.get("used_slots") == 0,
                        "store_admission_missing")
                stage = "manager_diag"
                result["manager_diag"] = inspect_manager_diag(
                    session["evidence"] / "manager-state", facts["run_id"])
                require(result["manager_diag"]["status"] == "PASS_MANAGER_DIAG_SURVIVAL",
                        "manager_diag_survival_missing")
                stage = "manager_close"
        result["credential_cleanup"] = "PASS"
        result["status"] = "PASS_MANAGER_PROJECT_CLOSURE"
    except Exception as exc:
        result["failed_stage"] = stage
        result["error_type"] = type(exc).__name__
        if isinstance(exc, ClosureCheckError):
            result["failure_reason"] = str(exc)
    finally:
        write_json_new(evidence / "result.json", result)
    return result


args_ref = []


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run", help="One memory-only onboarding credential lifetime; one project dispatch")
    run.add_argument("--evidence-dir", required=True)
    run.add_argument("--host", required=True)
    run.add_argument("--root-user", required=True)
    run.add_argument("--model-base-url", required=True)
    run.add_argument("--model", required=True)
    run.add_argument("--package", required=True)
    run.add_argument("--package-manifest", required=True)
    run.add_argument("--document", required=True)
    run.add_argument("--secrets-stdin", action="store_true", required=True)
    action = commands.add_parser("action", help="Control the owned Manager window after visual review")
    action.add_argument("--evidence-dir", required=True)
    action.add_argument("--action", required=True, choices=("click", "type-field", "provision", "type-document", "capture", "dispatch", "abort"))
    action.add_argument("--field", choices=("host", "root-user", "root-password", "model-base-url", "model-token", "model"))
    action.add_argument("--x", type=int)
    action.add_argument("--y", type=int)
    args = parser.parse_args(argv)
    try:
        if args.command == "action":
            control = Path(args.evidence_dir).resolve() / "controls"
            require(control.is_dir(), "live_manager_controls_missing")
            index = len(list(control.glob("command-*.json"))) + 1
            require(args.action != "type-field" or args.field is not None, "onboarding_field_required")
            write_json_new(control / f"command-{index:04d}.json", {"action": args.action, "field": args.field, "x": args.x, "y": args.y})
            print(json.dumps({"status": "GUI_COMMAND_QUEUED", "sequence": index}))
            return 0
        args_ref.append(args)
        evidence = Path(args.evidence_dir).resolve()
        result = run_project_closure(evidence,
            lambda: stdin_secrets_lease(sys.stdin.buffer),
            lambda secrets, ev: prepare_onboarding(args_ref[0], secrets, ev),
            manager_session,
            drive_manager,
            wait_for_admission)
        print(json.dumps(result, ensure_ascii=True))
        return 0 if result["status"] == "PASS_MANAGER_PROJECT_CLOSURE" else 2
    except Exception as exc:
        print(json.dumps({"status": "BLOCKED_OR_FAILED", "error_type": type(exc).__name__}), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
