"""Black-box M1 contract tests for the isolated ``agentranchd.exe``.

Boundary rules enforced here (see the M1 design and Task 9 brief):

* The daemon executable path is supplied explicitly through ``AGENTRANCHD_EXE``.
  The harness never searches for, or silently reuses, a packaged binary; a
  missing path fails every test instead of skipping.
* Every case runs in a fresh directory below ``<repo>/target/task-9-api``.
  System temp and AppData are asserted to be outside that root.
* Client traffic uses only ``http.client`` against ``127.0.0.1``. Proxy
  variables are stripped from the child environment; the suite never contacts
  an external host.
* The daemon is stopped with ``CTRL_BREAK_EVENT`` in its own process group
  (the same console signal the product handler installs), never by faking a
  clean exit; teardown force-kills only after an honest failure.
* Assertions read the daemon's own SQLite ledgers and the discovery file so
  the result is not just the API echoing itself.

Nothing here writes product state outside the per-case root, calls a model, or
provisions a host over SSH.
"""
from __future__ import annotations

import ctypes
import hashlib
import http.client
import json
import os
import re
import socket
import sqlite3
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

import pytest

pytestmark = pytest.mark.skipif(
    os.name != "nt", reason="the isolated daemon is a Windows console executable"
)

REPO = Path(__file__).resolve().parents[2]
SANDBOX = REPO / "target" / "task-9-api"
CREATE_NEW_PROCESS_GROUP = 0x00000200
CTRL_BREAK_EVENT = 1
STARTUP_DEADLINE = 30.0
UUID_RE = re.compile(r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
PROJECT_KEYS = {
    "id",
    "name",
    "phase",
    "project_path",
    "product_path",
    "document",
    "tasks",
    "actions",
    "blockers",
    "can_confirm",
    "can_split",
    "can_start",
    "can_test",
    "can_repair",
    "can_accept",
}
HEALTH_KEYS = {"contract", "app_version", "mode", "pid", "started_at", "instance_id", "profile"}
ENDPOINT_KEYS = {"schema", "url", "pid", "started_at", "instance_id"}
COMMAND_RECEIPT_KEYS = {"request_id", "result"}
# Projection rule of the published snapshot in Real mode: the receipt carries
# the command contract, while the snapshot additionally blocks dispatch until a
# signed readiness probe passes (service.rs project projection).
READINESS_BLOCKER = "真实节点未通过当前签名 readiness probe/ready；配置有效不等于可派工"


# ---------------------------------------------------------------------------
# Path and environment guards
# ---------------------------------------------------------------------------


def daemon_executable() -> Path:
    raw = os.environ.get("AGENTRANCHD_EXE", "")
    if not raw:
        pytest.fail(
            "AGENTRANCHD_EXE must name the absolute agentranchd.exe built by Cargo; "
            "the harness never discovers or reuses a packaged binary"
        )
    executable = Path(raw)
    if not executable.is_absolute() or not executable.is_file():
        pytest.fail(f"AGENTRANCHD_EXE={raw!r} is not an absolute existing file")
    return executable


def assert_isolated_root(root: Path) -> None:
    resolved = root.resolve()
    target_root = (REPO / "target").resolve()
    if resolved != target_root and target_root not in resolved.parents:
        raise AssertionError(f"test root {resolved} is outside {target_root}")
    # The checkout itself lives under the user profile; AppData and the system
    # temp roots are what must never be touched.
    for name in ("LOCALAPPDATA", "APPDATA", "TEMP", "TMP"):
        value = os.environ.get(name)
        if not value:
            continue
        blocked = Path(value).resolve()
        if resolved == blocked or blocked in resolved.parents:
            raise AssertionError(f"test root {resolved} is inside {name}={blocked}")
    if any(part.lower() == "appdata" for part in resolved.parts):
        raise AssertionError(f"test root {resolved} contains an AppData component")


def fresh_case_root() -> Path:
    SANDBOX.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="case-", dir=SANDBOX))
    assert_isolated_root(root)
    return root


# ---------------------------------------------------------------------------
# HTTP / SSE client
# ---------------------------------------------------------------------------


class Response:
    def __init__(self, status: int, headers: dict[str, str], body: bytes):
        self.status = status
        self.headers = headers
        self.body = body

    @property
    def media_type(self) -> str:
        return self.headers.get("content-type", "").split(";")[0].strip().lower()

    def payload(self):
        if self.media_type != "application/json":
            raise AssertionError(
                f"expected application/json, got {self.headers.get('content-type')!r}: {self.body[:200]!r}"
            )
        return json.loads(self.body.decode("utf-8"))

    def error_code(self) -> str:
        payload = self.payload()
        if set(payload) != {"error"}:
            raise AssertionError(f"error envelope has unexpected keys: {sorted(payload)}")
        error = payload["error"]
        if set(error) != {"code", "message", "next_step"}:
            raise AssertionError(f"error body has unexpected keys: {sorted(error)}")
        return error["code"]


class SseFrame:
    __slots__ = ("id", "event", "data", "raw")

    def __init__(self, id_: str | None, event: str | None, data, raw: str):
        self.id = id_
        self.event = event
        self.data = data
        self.raw = raw

    @property
    def seq(self) -> int:
        assert self.id is not None, f"frame {self.event!r} has no id"
        return int(self.id.split(":", 1)[1])

    @property
    def instance_id(self) -> str:
        assert self.id is not None
        return self.id.split(":", 1)[0]


class SseStream:
    """Bounded SSE reader over one ``http.client`` connection."""

    def __init__(self, daemon: "Daemon", query: str = "", headers: dict[str, str] | None = None):
        self.connection = http.client.HTTPConnection("127.0.0.1", daemon.port, timeout=15.0)
        self.connection.putrequest("GET", "/v1/events" + query)
        self.connection.putheader("Accept", "text/event-stream")
        for name, value in (headers or {}).items():
            self.connection.putheader(name, value)
        self.connection.endheaders()
        try:
            self.response = self.connection.getresponse()
        except OSError as error:
            self.connection.close()
            raise AssertionError(f"SSE request failed before headers: {error}") from error
        if self.response.status != 200:
            body = self.response.read()
            self.connection.close()
            raise AssertionError(f"events route answered {self.response.status}: {body!r}")
        content_type = self.response.getheader("Content-Type", "")
        if not content_type.startswith("text/event-stream"):
            self.connection.close()
            raise AssertionError(f"events route content type is {content_type!r}")
        self.socket = self.connection.sock
        self.closed = False

    def next_frame(self, deadline: float) -> SseFrame:
        end = time.monotonic() + deadline
        lines: list[str] = []
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"no SSE frame within {deadline:.1f}s")
            self.socket.settimeout(remaining)
            try:
                line = self.response.readline()
            except (socket.timeout, TimeoutError) as error:
                raise TimeoutError(f"no SSE frame within {deadline:.1f}s") from error
            if line == b"":
                raise EOFError("SSE stream ended")
            text = line.decode("utf-8").rstrip("\r\n")
            if text == "":
                frame = self._parse(lines)
                if frame is None:
                    # Comment-only block (idle heartbeat) is not an event.
                    lines = []
                    continue
                return frame
            lines.append(text)

    @staticmethod
    def _parse(lines: list[str]) -> SseFrame | None:
        id_ = event = None
        data_lines: list[str] = []
        for line in lines:
            if line.startswith(":"):
                continue
            field, separator, value = line.partition(":")
            if not separator:
                raise AssertionError(f"SSE line without a colon: {line!r}")
            if value.startswith(" "):
                value = value[1:]
            if field == "id":
                id_ = value
            elif field == "event":
                event = value
            elif field == "data":
                data_lines.append(value)
            else:
                raise AssertionError(f"unexpected SSE field {field!r}")
        if id_ is None and event is None and not data_lines:
            return None
        data = json.loads("\n".join(data_lines)) if data_lines else None
        return SseFrame(id_, event, data, "\n".join(lines))

    def wait_event(self, *, kind: str | None = None, request_id: int | None = None, deadline: float = 15.0) -> SseFrame:
        end = time.monotonic() + deadline
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                raise AssertionError(
                    f"no event kind={kind!r} request_id={request_id!r} on the stream within {deadline:.1f}s"
                )
            frame = self.next_frame(remaining)
            if kind is not None and frame.event != kind:
                continue
            if request_id is not None:
                if not isinstance(frame.data, dict) or frame.data.get("request_id") != request_id:
                    continue
            return frame

    def wait_receipt(self, request_id: int, deadline: float = 15.0) -> dict:
        frame = self.wait_event(kind="command_result", request_id=request_id, deadline=deadline)
        if not isinstance(frame.data, dict) or "request_id" not in frame.data:
            raise AssertionError(f"receipt frame has unexpected data: {frame.raw!r}")
        return frame.data

    def try_frame(self, deadline: float) -> SseFrame | None:
        """Return the next frame, or ``None`` if none arrives in ``deadline``.

        A timed-out buffered reader is not reused afterwards.
        """
        try:
            return self.next_frame(deadline)
        except TimeoutError:
            return None

    def close(self) -> None:
        if not self.closed:
            self.closed = True
            try:
                self.response.close()
            finally:
                self.connection.close()


# ---------------------------------------------------------------------------
# Daemon process under test
# ---------------------------------------------------------------------------


class Daemon:
    def __init__(self, mode: str):
        if mode not in {"real", "local"}:
            raise ValueError(mode)
        self.executable = daemon_executable()
        self.mode = mode
        self.root = fresh_case_root()
        self.state = self.root / "state"
        self.stdout_path = self.root / "stdout.log"
        self.stderr_path = self.root / "stderr.log"
        self.exchanges: list[tuple[str, str, Response]] = []
        self.streams: list[SseStream] = []
        self.exit_code: int | None = None
        self.shutdown_seconds: float | None = None
        environment = os.environ.copy()
        for name in (
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
        ):
            environment.pop(name, None)
        command = [
            str(self.executable),
            "--state-dir",
            str(self.state),
            "--port",
            "0",
            f"--{mode}",
        ]
        with open(self.stdout_path, "wb") as stdout, open(self.stderr_path, "wb") as stderr:
            self.proc = subprocess.Popen(
                command,
                cwd=str(self.root),
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=stdout,
                stderr=stderr,
                creationflags=CREATE_NEW_PROCESS_GROUP,
            )
        self.endpoint = self._wait_endpoint()

    # -- discovery ---------------------------------------------------------
    def _wait_endpoint(self) -> dict:
        path = self.state / "api-endpoint.json"
        end = time.monotonic() + STARTUP_DEADLINE
        while time.monotonic() < end:
            if path.is_file():
                metadata = json.loads(path.read_text(encoding="utf-8"))
                if metadata.get("pid") != self.proc.pid:
                    raise AssertionError(
                        f"discovery pid {metadata.get('pid')!r} is not the started process {self.proc.pid}"
                    )
                return metadata
            code = self.proc.poll()
            if code is not None:
                raise AssertionError(
                    f"daemon exited with {code} before publishing discovery; stderr={self.stderr_text()!r}"
                )
            time.sleep(0.02)
        raise AssertionError(
            f"daemon did not publish discovery within {STARTUP_DEADLINE:.0f}s; stderr={self.stderr_text()!r}"
        )

    @property
    def port(self) -> int:
        match = re.fullmatch(r"http://127\.0\.0\.1:(\d+)", self.endpoint["url"])
        if match is None:
            raise AssertionError(f"discovery url is not a loopback URL: {self.endpoint['url']!r}")
        return int(match.group(1))

    def endpoint_bytes(self) -> bytes:
        return (self.state / "api-endpoint.json").read_bytes()

    # -- HTTP --------------------------------------------------------------
    def request(
        self,
        method: str,
        path: str,
        body=None,
        headers: dict[str, str] | None = None,
        timeout: float = 15.0,
    ) -> Response:
        wire = None
        merged = {"Accept": "application/json"}
        if body is not None:
            if isinstance(body, (dict, list)):
                wire = json.dumps(body, ensure_ascii=True).encode("utf-8")
            elif isinstance(body, str):
                wire = body.encode("utf-8")
            else:
                wire = body
            merged["Content-Type"] = "application/json"
        if headers:
            merged.update(headers)
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=timeout)
        try:
            connection.request(method, path, body=wire, headers=merged)
            response = connection.getresponse()
            data = response.read()
            result = Response(
                response.status,
                {name.lower(): value for name, value in response.getheaders()},
                data,
            )
        finally:
            connection.close()
        self.exchanges.append((method, path, result))
        return result

    def command(self, kind: str, args: dict) -> Response:
        return self.request("POST", "/v1/commands", {"kind": kind, "args": args})

    def accept(self, kind: str, args: dict) -> int:
        response = self.command(kind, args)
        if response.status != 202:
            raise AssertionError(f"{kind} was not accepted: {response.status} {response.body[:300]!r}")
        envelope = response.payload()
        if envelope.get("accepted") is not True:
            raise AssertionError(f"{kind} envelope is not an acceptance: {envelope}")
        if envelope.get("instance_id") != self.endpoint["instance_id"]:
            raise AssertionError(f"{kind} envelope instance does not match discovery: {envelope}")
        request_id = envelope.get("request_id")
        if not isinstance(request_id, int) or isinstance(request_id, bool):
            raise AssertionError(f"{kind} envelope has no integer request id: {envelope}")
        return request_id

    def snapshot(self) -> tuple[int, dict[str, str], dict]:
        response = self.request("GET", "/v1/snapshot")
        if response.status != 200:
            raise AssertionError(f"snapshot answered {response.status}: {response.body[:300]!r}")
        return response.status, response.headers, response.payload()

    def wait_snapshot(self, predicate, description: str, deadline: float = 15.0) -> dict:
        end = time.monotonic() + deadline
        last: dict | None = None
        while time.monotonic() < end:
            _, _, last = self.snapshot()
            if predicate(last):
                return last
            time.sleep(0.05)
        raise AssertionError(f"{description} not reached within {deadline:.1f}s; last={last!r}")

    def open_sse(self, query: str = "", headers: dict[str, str] | None = None) -> SseStream:
        stream = SseStream(self, query=query, headers=headers)
        self.streams.append(stream)
        return stream

    def close_sse(self) -> None:
        for stream in self.streams:
            stream.close()
        self.streams.clear()

    # -- independent durable readback -------------------------------------
    def project_rows(self) -> dict[str, dict]:
        database = self.state / "project-workflow" / "controller.sqlite"
        deadline = time.monotonic() + 10.0
        while True:
            try:
                connection = sqlite3.connect(
                    f"file:{database}?mode=ro", uri=True, timeout=5.0
                )
                try:
                    rows = connection.execute("SELECT id, body FROM projects ORDER BY rowid").fetchall()
                finally:
                    connection.close()
                return {row[0]: json.loads(row[1]) for row in rows}
            except sqlite3.OperationalError as error:
                if time.monotonic() >= deadline:
                    raise AssertionError(f"project ledger unreadable: {error}") from error
                time.sleep(0.05)

    # -- logs / shutdown ---------------------------------------------------
    def stderr_text(self) -> str:
        try:
            return self.stderr_path.read_text(encoding="utf-8", errors="replace")
        except FileNotFoundError:
            return ""

    def stdout_bytes(self) -> bytes:
        return self.stdout_path.read_bytes()

    def stop(self, timeout: float = 15.0) -> int:
        if self.exit_code is not None:
            return self.exit_code
        if self.proc.poll() is not None:
            self.exit_code = self.proc.returncode
            return self.exit_code
        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        generate = kernel32.GenerateConsoleCtrlEvent
        generate.argtypes = [ctypes.c_ulong, ctypes.c_ulong]
        generate.restype = ctypes.c_int
        if not generate(CTRL_BREAK_EVENT, self.proc.pid):
            raise OSError(
                ctypes.get_last_error(),
                "GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) failed; the console "
                "signal required by the product shutdown path could not be delivered",
            )
        started = time.monotonic()
        try:
            self.exit_code = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait(timeout=5)
            raise AssertionError(f"daemon did not exit within {timeout:.0f}s after Ctrl+Break")
        self.shutdown_seconds = time.monotonic() - started
        return self.exit_code

    def teardown(self) -> None:
        self.close_sse()
        if self.proc.poll() is None:
            self.proc.kill()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


@pytest.fixture
def daemon():
    instances: list[Daemon] = []

    def start(mode: str = "real") -> Daemon:
        instance = Daemon(mode)
        instances.append(instance)
        return instance

    try:
        yield start
    finally:
        for instance in instances:
            instance.teardown()


# ---------------------------------------------------------------------------
# Helpers shared by tests
# ---------------------------------------------------------------------------


def hex_sha256(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def expected_snapshot_projection(receipt_project: dict) -> dict:
    """Derive the published snapshot project from the command receipt.

    Real mode is NotReady by construction, so the snapshot additionally carries
    the fixed readiness blocker and suppresses the four dispatch flags. Every
    other field, including the managed paths, must be identical.
    """
    projected = json.loads(json.dumps(receipt_project))
    if READINESS_BLOCKER not in projected["blockers"]:
        projected["blockers"].append(READINESS_BLOCKER)
    for flag in ("can_split", "can_start", "can_test", "can_repair"):
        projected[flag] = False
    return projected


def netstat_rows(pid: int) -> list[tuple[str, str, str]]:
    completed = subprocess.run(
        ["netstat", "-ano", "-p", "TCP"],
        capture_output=True,
        text=True,
        timeout=60,
    )
    rows: list[tuple[str, str, str]] = []
    for line in completed.stdout.splitlines():
        columns = line.split()
        if len(columns) < 4 or columns[0].upper() != "TCP":
            continue
        if columns[-1] != str(pid):
            continue
        state = columns[3] if len(columns) >= 5 else ""
        rows.append((columns[1], columns[2], state))
    return rows


def local_address_is_loopback(address: str) -> bool:
    return address.startswith("127.0.0.1:") or address.startswith("[::1]:")


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


def test_sse_parser_treats_heartbeat_comments_as_non_events():
    assert SseStream._parse([": keep-alive"]) is None
    frame = SseStream._parse(
        [": keep-alive", "id: 00000000-0000-4000-8000-000000000000:1", "event: snapshot", 'data: {"ok":true}']
    )
    assert frame is not None
    assert frame.event == "snapshot"
    assert frame.id == "00000000-0000-4000-8000-000000000000:1"
    assert frame.data == {"ok": True}
    with pytest.raises(AssertionError):
        SseStream._parse(["uptime: 5"])


def test_discovery_health_initial_snapshot_and_loopback_only(daemon):
    child = daemon("real")

    # Discovery file: one fresh isolated instance, loopback URL, own pid.
    endpoint = child.endpoint
    assert set(endpoint) == ENDPOINT_KEYS
    assert endpoint["schema"] == "api-endpoint/1"
    assert re.fullmatch(r"http://127\.0\.0\.1:\d+", endpoint["url"])
    assert endpoint["pid"] == child.proc.pid
    assert isinstance(endpoint["started_at"], int) and endpoint["started_at"] > 1_600_000_000
    assert UUID_RE.fullmatch(endpoint["instance_id"])
    assert child.port > 0

    # Health: the contract identity clients verify.
    health = child.request("GET", "/v1/health")
    assert health.status == 200
    assert health.media_type == "application/json"
    assert health.headers.get("cache-control") == "no-store"
    body = health.payload()
    assert set(body) == HEALTH_KEYS
    assert body["contract"] == 1
    assert re.fullmatch(r"\d+\.\d+\.\d+", body["app_version"])
    assert body["mode"] == "real"
    assert body["pid"] == child.proc.pid
    assert body["instance_id"] == endpoint["instance_id"]
    assert body["profile"] == "isolated"
    assert isinstance(body["started_at"], int) and body["started_at"] > 0

    # Initial snapshot: Real mode is NotReady by construction, never a fixture.
    status, headers, snapshot = child.snapshot()
    assert status == 200
    assert headers.get("x-agentranch-instance") == endpoint["instance_id"]
    assert headers.get("x-agentranch-seq", "").isdigit()
    assert snapshot["mode"] == "real"
    assert snapshot["readiness"] == "not_ready"
    assert snapshot["running"] is True
    assert snapshot["projects"] == []
    assert snapshot["dependencies"][0]["id"] == "real-ports"
    assert all(item["id"] != "native-local-fixture" for item in snapshot["dependencies"])

    # The OS view agrees with the product contract: this pid listens on loopback.
    rows = netstat_rows(child.proc.pid)
    assert rows, "netstat reported no sockets for the daemon pid"
    for local, foreign, state in rows:
        assert local_address_is_loopback(local), f"daemon socket is not loopback: {local} {foreign} {state}"
        if state.upper() == "ESTABLISHED":
            assert local_address_is_loopback(foreign), f"daemon connected externally: {foreign}"

    # No endpoint file was invented before the listener was ready is not
    # observable after the fact; the published file must resolve instead.
    assert child.request("GET", "/v1/not-a-route").status == 404


def test_sse_checkpoint_then_project_create_is_read_back(daemon):
    child = daemon("real")

    # SSE first: a subscriber receives the published snapshot before any
    # command is submitted. A subscriber in the first milliseconds can catch a
    # checkpoint at sequence 0 (the bridge's first frame is still pending), so
    # the checkpoint is only required to carry the initial snapshot.
    stream = child.open_sse()
    checkpoint = stream.next_frame(15.0)
    assert checkpoint.event == "snapshot"
    assert checkpoint.id is not None
    assert checkpoint.instance_id == child.endpoint["instance_id"]
    assert checkpoint.data["projects"] == []
    assert checkpoint.data["mode"] == "real"
    assert checkpoint.seq >= 0

    # With the daemon idle, a checkpoint is exactly the current published head
    # and its payload is the stored snapshot (the replay cursor for later).
    _, snap_headers, latest = child.snapshot()
    head_before = int(snap_headers["x-agentranch-seq"])
    steady = child.open_sse()
    checkpoint = steady.next_frame(15.0)
    assert checkpoint.event == "snapshot"
    assert checkpoint.seq == head_before
    assert checkpoint.data == latest
    cursor = checkpoint.id

    request_id = child.accept("project_create", {"name": "m1 black box"})
    receipt = stream.wait_receipt(request_id)
    assert set(receipt) == COMMAND_RECEIPT_KEYS
    assert receipt["request_id"] == request_id
    created = receipt["result"]
    assert set(created) == PROJECT_KEYS
    assert UUID_RE.fullmatch(created["id"])
    assert created["name"] == "m1 black box"
    assert created["phase"] == "needs_document"
    assert created["document"] is None
    assert created["can_confirm"] is False

    # The same accepted command advanced the published snapshot.
    snapshot = child.wait_snapshot(
        lambda snap: any(project["id"] == created["id"] for project in snap["projects"]),
        "created project in snapshot",
    )
    projected = next(project for project in snapshot["projects"] if project["id"] == created["id"])
    assert projected == expected_snapshot_projection(created)
    assert set(projected) == PROJECT_KEYS

    # Independent durable readback: the ledger row equals the receipt result.
    rows = child.project_rows()
    assert rows[created["id"]] == created

    # Same instance, cursor captured before submission: replay yields the real
    # receipts with monotonic ids and does not re-execute anything.
    replay = child.open_sse(headers={"Last-Event-ID": cursor})
    replayed = []
    while True:
        frame = replay.next_frame(5.0)
        replayed.append(frame)
        if frame.event == "command_result" and isinstance(frame.data, dict) and frame.data.get("request_id") == request_id:
            break
    assert [frame.seq for frame in replayed] == sorted({frame.seq for frame in replayed})
    assert all(frame.seq > int(cursor.split(":", 1)[1]) for frame in replayed)
    assert replayed[-1].data["result"] == created
    assert child.project_rows() == rows

    # `after=0` replays the full retained ring, while no cursor is only a
    # checkpoint; a late subscriber without a cursor gets no old receipts.
    head = int(child.snapshot()[1]["x-agentranch-seq"])
    full = child.open_sse(query=f"?instance_id={child.endpoint['instance_id']}&after=0")
    sequences = [full.next_frame(5.0).seq for _ in range(head)]
    assert sequences == list(range(1, head + 1))

    late = child.open_sse()
    late_checkpoint = late.next_frame(15.0)
    assert late_checkpoint.event == "snapshot"
    assert late_checkpoint.seq == head
    assert late.try_frame(0.6) is None

    # An unknown instance is answered with a gap frame, then a checkpoint.
    gap = child.open_sse(query=f"?instance_id={uuid.uuid4()}&after=0")
    gap_frame = gap.next_frame(15.0)
    assert gap_frame.event == "gap"
    assert gap_frame.data["reason"] == "instance_changed"
    assert gap_frame.data["instance_id"] == child.endpoint["instance_id"]
    assert gap.next_frame(5.0).event == "snapshot"

    # Malformed cursors are fixed 400s, not partial streams.
    assert child.request("GET", "/v1/events?after=5").status == 400
    assert child.request("GET", "/v1/events?instance_id=not-a-uuid&after=0").status == 400
    future = int(child.snapshot()[1]["x-agentranch-seq"]) + 1000
    assert child.request(
        "GET", f"/v1/events?instance_id={child.endpoint['instance_id']}&after={future}"
    ).status == 400
    assert child.request("POST", "/v1/events").status == 405


def test_import_confirm_artifacts_and_negative_boundaries(daemon):
    child = daemon("real")
    stream = child.open_sse()
    assert stream.next_frame(15.0).event == "snapshot"

    created = stream.wait_receipt(child.accept("project_create", {"name": "import case"}))["result"]
    project_id = created["id"]
    import_dir = child.state / "imports"
    content = b"# M1 import\n\n- keep the transport boundary explicit\n"
    document = import_dir / "design.md"
    document.write_bytes(content)
    digest = hex_sha256(content)

    # Staging failures are known non-acceptance: no request id is issued.
    for bad_args in (
        {"project_id": project_id, "path": "missing.md"},
        {"project_id": project_id, "path": "../escape.md"},
        {"project_id": project_id, "path": str(document)},
        {"project_id": project_id, "path": "design.bin"},
    ):
        response = child.command("project_import", bad_args)
        assert response.status == 422, f"{bad_args} answered {response.status} {response.body[:200]!r}"
        assert response.error_code() == "invalid_import"
        assert "request_id" not in response.payload()

    imported = stream.wait_receipt(
        child.accept("project_import", {"project_id": project_id, "path": "design.md"})
    )["result"]
    assert imported["id"] == project_id
    assert imported["phase"] == "document_review"
    document_receipt = imported["document"]
    assert document_receipt["version"] == 1
    assert document_receipt["filename"] == "design.md"
    assert document_receipt["sha256"] == digest
    assert document_receipt["text"] == content.decode("utf-8")
    assert document_receipt["confirmed"] is False
    assert imported["can_confirm"] is True
    assert child.project_rows()[project_id]["document"] == document_receipt

    # A wrong hash is accepted by the bridge but rejected by the core: the
    # receipt is the real business answer, not the 202.
    wrong = child.accept(
        "project_confirm", {"project_id": project_id, "version": 1, "sha256": "0" * 64}
    )
    rejected = stream.wait_receipt(wrong)
    assert rejected["request_id"] == wrong
    assert rejected["error"]["code"] == "project_action_rejected"
    assert child.project_rows()[project_id]["document"]["confirmed"] is False

    confirmed = stream.wait_receipt(
        child.accept("project_confirm", {"project_id": project_id, "version": 1, "sha256": digest})
    )["result"]
    assert confirmed["document"]["confirmed"] is True
    assert confirmed["phase"] == "confirmed"
    assert confirmed["can_split"] is True
    snapshot = child.wait_snapshot(
        lambda snap: any(
            project["id"] == project_id and project["document"]["confirmed"]
            for project in snap["projects"]
        ),
        "confirmed project in snapshot",
    )
    projected = next(project for project in snapshot["projects"] if project["id"] == project_id)
    assert projected["phase"] == "confirmed"
    assert projected["document"]["sha256"] == digest

    # Second confirm is a real rejection, still over the same stream.
    again = child.accept(
        "project_confirm", {"project_id": project_id, "version": 1, "sha256": digest}
    )
    assert stream.wait_receipt(again)["error"]["code"] == "project_action_rejected"

    # Artifact readback from the trusted product root.
    product = child.state / "projects" / project_id / "product"
    artifact = product / "output" / "report.txt"
    artifact.parent.mkdir(parents=True)
    artifact_bytes = b"artifact-bytes-0123456789\n"
    artifact.write_bytes(artifact_bytes)
    served = child.request("GET", f"/v1/artifacts/{project_id}/output/report.txt")
    assert served.status == 200
    assert served.body == artifact_bytes
    assert served.media_type == "application/octet-stream"
    assert served.headers.get("content-disposition") == "attachment"
    assert served.headers.get("x-content-type-options") == "nosniff"
    assert served.headers.get("cache-control") == "no-store"

    # Every illegal or missing artifact case is the same fixed 404.
    state_marker = str(child.state)
    for path in (
        f"/v1/artifacts/{project_id}/output/missing.txt",
        f"/v1/artifacts/{uuid.uuid4()}/output/report.txt",
        f"/v1/artifacts/{project_id.upper()}/output/report.txt",
        "/v1/artifacts/not-a-uuid/output/report.txt",
        f"/v1/artifacts/{project_id}/../api-endpoint.json",
        f"/v1/artifacts/{project_id}/output",
        f"/v1/artifacts/{project_id}",
        "/v1/artifacts/",
    ):
        response = child.request("GET", path)
        assert response.status == 404, f"{path} answered {response.status}"
        assert response.error_code() == "not_found"
        assert state_marker.lower() not in response.body.decode("utf-8", "replace").lower()
    assert child.request("POST", f"/v1/artifacts/{project_id}/output/report.txt").status == 405

    # The import source is left untouched; only the private copy is managed.
    assert document.read_bytes() == content


def test_settings_canary_round_trip_and_no_secret_leak(daemon):
    child = daemon("real")
    canary = "canary-" + uuid.uuid4().hex + "-must-not-leak"
    settings = {
        "schema": 2,
        "model": {
            "base_url": "https://127.0.0.1:9",
            "api_key": canary,
            "model": "canary-model",
        },
        "workers": [
            {
                "id": "worker-split",
                "role": "split",
                "host": "127.0.0.1",
                "username": "root",
                "password": canary,
            }
        ],
    }

    missing = child.request("GET", "/v1/settings/connection")
    assert missing.status == 404
    assert missing.error_code() == "not_found"

    saved = child.request("PUT", "/v1/settings/connection", settings)
    assert saved.status == 200
    assert saved.payload() == {"saved": True}
    loaded = child.request("GET", "/v1/settings/connection")
    assert loaded.status == 200
    assert loaded.payload() == settings

    # Invalid schema-2 content is a fixed 422 that repeats nothing.
    invalid = json.loads(json.dumps(settings))
    invalid["workers"][0]["host"] = "not-an-ip"
    rejected = child.request("PUT", "/v1/settings/connection", invalid)
    assert rejected.status == 422
    assert rejected.error_code() == "invalid_settings"
    assert canary not in rejected.body.decode("utf-8", "replace")

    assert child.request("PUT", "/v1/settings/connection", "{not json").status == 400
    assert child.request("PUT", "/v1/settings/connection", settings, headers={"Content-Type": "text/plain"}).status == 415
    assert child.request("PATCH", "/v1/settings/connection").status == 405

    # A disabled command that carries a secret argument answers the fixed 403.
    disabled = child.command(
        "provision_worker",
        {
            "host": "192.0.2.10",
            "root_user": "root",
            "root_password": canary,
            "model_base_url": "https://127.0.0.1:9",
            "model_token": canary,
            "model": "canary-model",
        },
    )
    assert disabled.status == 403
    assert disabled.error_code() == "command_disabled"
    assert canary not in disabled.body.decode("utf-8", "replace")

    # The secret never reaches the event stream, health, snapshot or discovery.
    stream = child.open_sse()
    checkpoint = stream.next_frame(15.0)
    assert checkpoint.event == "snapshot"
    assert canary not in checkpoint.raw
    snapshot = child.request("GET", "/v1/snapshot").body.decode("utf-8", "replace")
    health = child.request("GET", "/v1/health").body.decode("utf-8", "replace")
    assert canary not in snapshot
    assert canary not in health
    assert canary not in child.endpoint_bytes().decode("utf-8", "replace")

    assert child.stop() == 0
    written = child.stderr_text() + child.stdout_bytes().decode("utf-8", "replace")
    assert canary not in written
    # The settings route itself must echo the stored value; every other
    # recorded exchange must not carry the canary.
    for method, path, response in child.exchanges:
        if (method, path) in {("PUT", "/v1/settings/connection"), ("GET", "/v1/settings/connection")}:
            continue
        assert canary not in response.body.decode("utf-8", "replace"), (method, path)

    # The plaintext file exists by design and is the only place it is stored.
    assert canary in (child.state / "connection-settings.json").read_text(encoding="utf-8")


def test_disabled_commands_and_fixed_error_envelopes(daemon):
    child = daemon("real")

    for kind, args in (
        ("dispatch", {"template": "sum-native-v1"}),
        ("status", {"run_id": "r"}),
        ("approvals", {"run_id": "r"}),
        ("decide", {"run_id": "r", "attempt": 1, "session": "s", "request_id": "q", "generation": 1, "scope_hash": "h", "decision_id": "allow_once"}),
        ("report", {"run_id": "r"}),
        ("retry", {"run_id": "r", "attempt": 1}),
        ("cancel", {"run_id": "r"}),
        ("provision_worker", {"host": "192.0.2.10", "root_user": "root", "root_password": "p", "model_base_url": "https://127.0.0.1:9", "model_token": "t", "model": "m"}),
    ):
        response = child.command(kind, args)
        assert response.status == 403, f"{kind} answered {response.status}"
        assert response.error_code() == "command_disabled"
        assert "accepted" not in response.payload()

    unknown = child.command("frobnicate", {})
    assert unknown.status == 400
    assert unknown.error_code() == "bad_request"
    assert child.request("POST", "/v1/commands", {"kind": "project_create"}).status == 400
    assert child.request("POST", "/v1/commands", '{"kind":"project_create","args":[]}').status == 400
    assert child.request("POST", "/v1/commands", {"kind": "project_create", "args": {"name": "ok"}, "extra": 1}).status == 400

    assert child.request("POST", "/v1/commands", {"kind": "project_create", "args": {"name": "x"}}, headers={"Content-Type": "text/plain"}).status == 415
    assert child.request("GET", "/v1/commands").status == 405
    assert child.request("GET", "/v1/health", headers={}).status == 200
    assert child.request("POST", "/v1/health").status == 405
    assert child.request("PUT", "/v1/snapshot").status == 405

    oversized = child.request(
        "POST",
        "/v1/commands",
        headers={"Content-Type": "application/json", "Content-Length": str(1024 * 1024 + 1)},
    )
    assert oversized.status == 413
    assert oversized.error_code() == "payload_too_large"

    # No error body echoes a request value.
    for method, path, response in child.exchanges:
        if response.status >= 400:
            text = response.body.decode("utf-8", "replace")
            assert "192.0.2.10" not in text
            assert "sum-native-v1" not in text


def test_real_start_local_refuses_without_starting_a_fixture(daemon):
    child = daemon("real")
    stream = child.open_sse()
    assert stream.next_frame(15.0).event == "snapshot"

    request_id = child.accept("start_local", {})
    receipt = stream.wait_receipt(request_id)
    assert receipt["request_id"] == request_id
    assert receipt["error"]["code"] == "real_not_ready"
    assert "本地替代" in receipt["error"]["message"]

    snapshot = child.wait_snapshot(
        lambda snap: snap["readiness"] == "not_ready", "readiness remains not_ready"
    )
    assert snapshot["running"] is True
    assert all(item["id"] != "native-local-fixture" for item in snapshot["dependencies"])
    assert all(item["id"] != "local-fixture" for item in snapshot["dependencies"])

    # The fixture runtime leaves these behind when it ever opens; Real mode
    # must not have touched them.
    assert not (child.root / "local-host.json").exists()
    assert not (child.state / "local-host.json").exists()
    assert not (child.state / "fixtures").exists()
    entries = {path.name for path in child.root.iterdir()}
    assert entries <= {"state", "stdout.log", "stderr.log"}, entries


def test_duplicate_posts_distinct_requests_and_reader_disconnect_keeps_work(daemon):
    child = daemon("real")
    stream = child.open_sse()
    checkpoint = stream.next_frame(15.0)
    assert checkpoint.event == "snapshot"
    cursor = checkpoint.id

    first = child.accept("project_create", {"name": "duplicate payload"})
    second = child.accept("project_create", {"name": "duplicate payload"})
    assert first != second
    first_result = stream.wait_receipt(first)["result"]
    second_result = stream.wait_receipt(second)["result"]
    assert first_result["id"] != second_result["id"]
    assert len(child.project_rows()) == 2

    # Closing the reader is not cancellation: accepted work still completes.
    stream.close()
    third = child.accept("project_create", {"name": "after reader closed"})
    third_row = child.wait_snapshot(
        lambda snap: any(project["name"] == "after reader closed" for project in snap["projects"]),
        "third project after the reader disconnected",
    )
    third_project = next(project for project in third_row["projects"] if project["name"] == "after reader closed")
    assert len(child.project_rows()) == 3

    # The receipt was published even though no stream was connected, and a
    # replay from the pre-submit cursor returns it without re-execution.
    replay = child.open_sse(headers={"Last-Event-ID": cursor})
    replay_ids = set()
    while third not in replay_ids:
        frame = replay.next_frame(5.0)
        if frame.event == "command_result" and isinstance(frame.data, dict):
            replay_ids.add(frame.data.get("request_id"))
    assert {first, second, third} <= replay_ids
    assert third_project["id"] in child.project_rows()
    assert len(child.project_rows()) == 3


def test_shutdown_drains_open_stream_removes_discovery_and_exits_clean(daemon):
    child = daemon("real")
    stream = child.open_sse()
    checkpoint = stream.next_frame(15.0)
    assert checkpoint.event == "snapshot"

    accepted = child.accept("project_create", {"name": "shutdown case"})
    assert stream.wait_receipt(accepted)["request_id"] == accepted

    started = time.monotonic()
    assert child.stop() == 0
    assert child.shutdown_seconds is not None
    assert time.monotonic() - started <= 10.0

    # Discovery is withdrawn and the port stops answering.
    assert not (child.state / "api-endpoint.json").exists()
    with pytest.raises(OSError):
        socket.create_connection(("127.0.0.1", child.port), timeout=2).close()

    # The open stream received the real stopped snapshot, then EOF.
    saw_stopped = False
    while True:
        try:
            frame = stream.next_frame(10.0)
        except EOFError:
            break
        if frame.event == "snapshot" and frame.data["running"] is False:
            assert frame.data["readiness"] == "stopped"
            saw_stopped = True
    assert saw_stopped

    stderr = child.stderr_text()
    assert "startup failed" not in stderr
    assert "exceeded its budget" not in stderr
