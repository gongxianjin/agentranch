//! Loopback HTTP/1.1 JSON and SSE transport.
//!
//! This module owns transport concerns only: routing, method/media/body
//! limits, the fixed application error envelope and SSE framing. It never
//! reads business state directly: commands go through the owner-thread-safe
//! [`BridgeClient`] and events come from the read-only [`EventHubAccess`].
//!
//! Nothing here logs or echoes a request URL, query, header, body, path or
//! raw error. Fixed diagnostics are the only outward text besides contract
//! payloads.

use crate::{
    api::{
        bridge::{BridgeClient, BridgeSubmitError},
        contract,
        events::{
            Cursor, CursorError, EventFrame, EventHub, PublishedView, SubscribeResult,
        },
        files::{self, ApiFileError, ImportError, ImportStage},
        lifecycle::Ingress,
    },
    config::Mode,
    connection_settings::{ConnectionSettings, ConnectionSettingsStore},
    core::service::UiCommand,
};
use bytes::Bytes;
use http_body_util::{combinators::UnsyncBoxBody, BodyExt, Full};
use hyper::{
    body::{Body, Frame, Incoming},
    header::{HeaderValue, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    http::{Request, Response, StatusCode, Version},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde_json::{json, Value};
use std::{
    convert::Infallible,
    fmt,
    path::PathBuf,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
};
use uuid::Uuid;

/// Streaming body cap for `POST`/`PUT` requests.
pub const BODY_LIMIT: usize = 1024 * 1024;
/// Total deadline for reading request headers.
pub const HEADER_DEADLINE: Duration = Duration::from_secs(5);
/// Total deadline for reading the whole request body.
pub const BODY_DEADLINE: Duration = Duration::from_secs(5);
/// Idle SSE heartbeat interval.
pub const DEFAULT_HEARTBEAT: Duration = Duration::from_secs(15);

const MALFORMED_BODY: &str =
    r#"{"error":{"code":"internal","message":"服务内部错误；结果未确认","next_step":"检查固定诊断信息后重试，不要自动重投已受理请求"}}"#;

type ResponseBody = UnsyncBoxBody<Bytes, Infallible>;

/// Immutable transport identity plus the two owner-thread-safe handles.
///
/// `state_dir` is the trusted isolated root for the settings and artifact
/// routes. Without it those routes answer with a fixed `unavailable` error;
/// production assembly attaches it once the lifecycle owns the directory.
pub struct HttpApp {
    instance_id: Uuid,
    mode: Mode,
    contract: u32,
    app_version: &'static str,
    pid: u32,
    started_at: u64,
    bridge: Arc<BridgeClient>,
    events: Arc<EventHubAccess>,
    heartbeat: Duration,
    state_dir: Option<PathBuf>,
    ingress: Option<Arc<Ingress>>,
}

impl HttpApp {
    /// The hub owns the transport instance identity. Health, snapshot headers
    /// and SSE event ids are all read by clients as one identity, so it is
    /// taken from the hub once instead of accepting a second, independently
    /// chosen value that could disagree with published frame ids.
    pub fn new(mode: Mode, bridge: Arc<BridgeClient>, events: Arc<EventHubAccess>) -> Self {
        let instance_id = events.instance_id();
        Self {
            instance_id,
            mode,
            contract: contract::CONTRACT_VERSION,
            app_version: env!("CARGO_PKG_VERSION"),
            pid: std::process::id(),
            started_at: unix_seconds(),
            bridge,
            events,
            heartbeat: DEFAULT_HEARTBEAT,
            state_dir: None,
            ingress: None,
        }
    }

    /// Test seam for the idle heartbeat. Production assembly keeps 15 seconds.
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat = interval;
        self
    }

    /// Attach the isolated state root used by the settings and artifact
    /// routes. The caller owns creation and validation of that directory.
    pub fn with_state_dir(mut self, state_dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(state_dir.into());
        self
    }

    /// Share the lifecycle ingress flag. Once shutdown closes it, new command
    /// submissions answer the fixed `unavailable` error before decoding, and
    /// the bridge refuses anything still queued.
    pub fn with_ingress(mut self, ingress: Arc<Ingress>) -> Self {
        self.ingress = Some(ingress);
        self
    }
}

/// Read-only view of the Task 3 `EventHub`. The mutable hub handle never
/// reaches routing code, so HTTP can only observe published state.
pub struct EventHubAccess {
    hub: Arc<Mutex<EventHub>>,
    changes: watch::Receiver<u64>,
    /// Shutdown flag shared with every open SSE stream. Publishing continues
    /// to work; this only ends transport streams, never business state.
    shutdown: watch::Sender<bool>,
}

impl EventHubAccess {
    pub fn new(hub: Arc<Mutex<EventHub>>) -> Self {
        let (sender, receiver) = watch::channel(0u64);
        {
            let mut hub = lock_hub(&hub);
            hub.set_change_notifier(Arc::new(move || {
                sender.send_modify(|version| *version = version.wrapping_add(1));
            }));
        }
        let (shutdown, _) = watch::channel(false);
        Self { hub, changes: receiver, shutdown }
    }

    /// Ends SSE streaming after the owner finished draining real receipts and
    /// the stopped snapshot. Idempotent; the hub itself stays readable.
    pub fn close(&self) {
        self.shutdown.send_modify(|closed| *closed = true);
    }

    pub fn is_closed(&self) -> bool {
        *self.shutdown.borrow()
    }

    pub fn instance_id(&self) -> Uuid {
        lock_hub(&self.hub).instance_id()
    }

    pub fn head(&self) -> u64 {
        lock_hub(&self.hub).head()
    }

    pub fn published_view(&self) -> PublishedView {
        lock_hub(&self.hub).published_view()
    }

    pub fn subscribe(&self, cursor: Option<Cursor>) -> Result<SubscribeResult, CursorError> {
        lock_hub(&self.hub).subscribe(cursor)
    }

    fn changes(&self) -> watch::Receiver<u64> {
        self.changes.clone()
    }
}

fn lock_hub(hub: &Arc<Mutex<EventHub>>) -> std::sync::MutexGuard<'_, EventHub> {
    hub.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Fixed transport failure. Deliberately carries no raw hyper/io error.
#[derive(Clone, Copy)]
pub struct HttpTransportError {
    category: &'static str,
}

impl HttpTransportError {
    fn new(category: &'static str) -> Self {
        Self { category }
    }

    pub fn category(&self) -> &'static str {
        self.category
    }
}

impl fmt::Debug for HttpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "HttpTransportError({})", self.category)
    }
}

impl fmt::Display for HttpTransportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "http transport error: {}", self.category)
    }
}

impl std::error::Error for HttpTransportError {}

/// Serves exactly one HTTP/1.1 connection. The caller owns accept/logging
/// policy; this function never reports request details in its error.
pub async fn serve_connection(
    stream: TcpStream,
    app: Arc<HttpApp>,
) -> Result<(), HttpTransportError> {
    let service = service_fn(move |request| {
        let app = app.clone();
        async move { Ok::<_, Infallible>(handle_request(request, app).await) }
    });
    http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_DEADLINE)
        .serve_connection(TokioIo::new(stream), service)
        .await
        .map_err(|_| HttpTransportError::new("connection"))
}

async fn handle_request(request: Request<Incoming>, app: Arc<HttpApp>) -> Response<ResponseBody> {
    if request.version() != Version::HTTP_11 {
        return bad_request();
    }
    let path = request.uri().path();
    if path == "/v1/health" {
        return health(&request, &app);
    }
    if path == "/v1/snapshot" {
        return snapshot(&request, &app);
    }
    if path == "/v1/commands" {
        return commands(request, app).await;
    }
    if path == "/v1/events" {
        return events(&request, &app);
    }
    if path == "/v1/settings/connection" {
        return settings(request, app).await;
    }
    if path.starts_with("/v1/artifacts/") {
        return artifact(request, app).await;
    }
    not_found()
}

fn health(request: &Request<Incoming>, app: &HttpApp) -> Response<ResponseBody> {
    if request.method() != hyper::Method::GET {
        return method_not_allowed();
    }
    json_response(
        StatusCode::OK,
        &json!({
            "contract": app.contract,
            "app_version": app.app_version,
            "mode": app.mode,
            "pid": app.pid,
            "started_at": app.started_at,
            "instance_id": app.instance_id.to_string(),
            "profile": "isolated",
        }),
    )
}

fn snapshot(request: &Request<Incoming>, app: &HttpApp) -> Response<ResponseBody> {
    if request.method() != hyper::Method::GET {
        return method_not_allowed();
    }
    let view = app.events.published_view();
    let bytes = match serde_json::to_vec(view.snapshot.as_ref()) {
        Ok(bytes) => bytes,
        Err(_) => {
            return response_with(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application/json",
                full_body(Bytes::from_static(MALFORMED_BODY.as_bytes())),
            );
        }
    };
    let mut response = response_with(
        StatusCode::OK,
        "application/json",
        full_body(Bytes::from(bytes)),
    );
    if let Ok(value) = HeaderValue::from_str(&view.instance_id.to_string()) {
        response.headers_mut().insert("x-agentranch-instance", value);
    }
    if let Ok(value) = HeaderValue::from_str(&view.head.to_string()) {
        response.headers_mut().insert("x-agentranch-seq", value);
    }
    response
}

async fn commands(request: Request<Incoming>, app: Arc<HttpApp>) -> Response<ResponseBody> {
    if request.method() != hyper::Method::POST {
        return method_not_allowed();
    }
    // Shutdown closes ingress first: no command may be decoded, staged or
    // queued once the lifecycle has begun stopping the core.
    if app.ingress.as_ref().is_some_and(|ingress| ingress.is_closed()) {
        return unavailable();
    }
    if !json_content_type(request.headers().get(CONTENT_TYPE)) {
        return unsupported_media_type();
    }
    if let Some(length) = request.headers().get(CONTENT_LENGTH) {
        match length.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
            Some(length) if length > BODY_LIMIT as u64 => return payload_too_large(),
            Some(_) => {}
            None => return bad_request(),
        }
    }
    let (_, body) = request.into_parts();
    let body = match tokio::time::timeout(BODY_DEADLINE, read_limited(body)).await {
        Ok(Ok(body)) => body,
        Ok(Err(BodyReadError::TooLarge)) => return payload_too_large(),
        Ok(Err(BodyReadError::Read)) => return bad_request(),
        Err(_) => return bad_request(),
    };
    let command = match contract::decode_command(&body) {
        Ok(command) => command,
        Err(_) => return bad_request(),
    };
    match command_policy(&command, &app.mode, app.state_dir.is_some()) {
        CommandPolicy::Allowed => {}
        CommandPolicy::Import => match stage_import_command(app.clone(), command).await {
            Ok((command, stage)) => {
                return match submit_import(app.bridge.clone(), command, stage).await {
                    Ok(request_id) => json_response(
                        StatusCode::ACCEPTED,
                        &json!({
                            "accepted": true,
                            "instance_id": app.instance_id.to_string(),
                            "request_id": request_id,
                        }),
                    ),
                    Err(BridgeSubmitError::Busy) => busy(),
                    Err(BridgeSubmitError::Stopped) => unavailable(),
                    Err(BridgeSubmitError::AcceptanceUnknown) => acceptance_unknown(),
                };
            }
            Err(response) => return response,
        },
        CommandPolicy::ImportDeferred => return import_deferred(),
        CommandPolicy::Disabled => return command_disabled(),
    }
    match submit(app.bridge.clone(), command).await {
        Ok(request_id) => json_response(
            StatusCode::ACCEPTED,
            &json!({
                "accepted": true,
                "instance_id": app.instance_id.to_string(),
                "request_id": request_id,
            }),
        ),
        Err(BridgeSubmitError::Busy) => busy(),
        Err(BridgeSubmitError::Stopped) => unavailable(),
        Err(BridgeSubmitError::AcceptanceUnknown) => acceptance_unknown(),
    }
}

/// Isolated schema 2 settings. GET returns the stored test values verbatim;
/// PUT validates and atomically replaces the single isolated test file.
async fn settings(request: Request<Incoming>, app: Arc<HttpApp>) -> Response<ResponseBody> {
    let method = request.method().clone();
    let Some(state_dir) = app.state_dir.clone() else {
        return match method {
            hyper::Method::GET | hyper::Method::PUT => route_deferred(),
            _ => method_not_allowed(),
        };
    };
    match method {
        hyper::Method::GET => {
            let store = ConnectionSettingsStore::new(&state_dir);
            match tokio::task::spawn_blocking(move || files::load_isolated_settings(&store)).await {
                Ok(Ok(Some(settings))) => match serde_json::to_value(&settings) {
                    Ok(value) => json_response(StatusCode::OK, &value),
                    Err(_) => internal(),
                },
                Ok(Ok(None)) => not_found(),
                Ok(Err(_)) | Err(_) => internal(),
            }
        }
        hyper::Method::PUT => {
            if !json_content_type(request.headers().get(CONTENT_TYPE)) {
                return unsupported_media_type();
            }
            if let Some(length) = request.headers().get(CONTENT_LENGTH) {
                match length.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
                    Some(length) if length > BODY_LIMIT as u64 => return payload_too_large(),
                    Some(_) => {}
                    None => return bad_request(),
                }
            }
            let (_, body) = request.into_parts();
            let body = match tokio::time::timeout(BODY_DEADLINE, read_limited(body)).await {
                Ok(Ok(body)) => body,
                Ok(Err(BodyReadError::TooLarge)) => return payload_too_large(),
                Ok(Err(BodyReadError::Read)) => return bad_request(),
                Err(_) => return bad_request(),
            };
            // Spec §5: malformed JSON is 400, but valid JSON whose schema or
            // values fail validation is 422 `invalid_settings`. `Data` covers
            // unknown fields (`deny_unknown_fields`), wrong types and missing
            // fields; `Io` cannot occur from a slice but maps to `internal`
            // rather than being silently reported as a client error.
            let settings: ConnectionSettings = match serde_json::from_slice(&body) {
                Ok(settings) => settings,
                Err(error) => {
                    return match error.classify() {
                        serde_json::error::Category::Syntax | serde_json::error::Category::Eof => bad_request(),
                        serde_json::error::Category::Data => invalid_settings(),
                        serde_json::error::Category::Io => internal(),
                    };
                }
            };
            let store = ConnectionSettingsStore::new(&state_dir);
            match tokio::task::spawn_blocking(move || {
                files::save_isolated_settings(&store, settings)
            })
            .await
            {
                Ok(Ok(())) => json_response(StatusCode::OK, &json!({"saved": true})),
                Ok(Err(ApiFileError::InvalidInput)) => invalid_settings(),
                Ok(Err(_)) | Err(_) => internal(),
            }
        }
        _ => method_not_allowed(),
    }
}

/// Read-only artifact below the trusted isolated product root. Every illegal,
/// linked, oversized or missing case is the same fixed 404.
async fn artifact(request: Request<Incoming>, app: Arc<HttpApp>) -> Response<ResponseBody> {
    if request.method() != hyper::Method::GET {
        return method_not_allowed();
    }
    let Some(state_dir) = app.state_dir.clone() else {
        return route_deferred();
    };
    let path = request.uri().path().to_owned();
    let Some(rest) = path.strip_prefix("/v1/artifacts/") else {
        return not_found();
    };
    let Some((project, relative)) = rest.split_once('/') else {
        return not_found();
    };
    if project.is_empty() || relative.is_empty() {
        return not_found();
    }
    let project = project.to_owned();
    let relative = relative.to_owned();
    // Spec §5 L123/L127: only a project registered in the published snapshot
    // may be read, and its root is derived from the snapshot's exact UUID plus
    // the trusted state directory. A UUID directory left behind by another
    // instance must not turn into a readable artifact.
    let Ok(project_id) = Uuid::parse_str(&project) else {
        return not_found();
    };
    let published = app.events.published_view();
    let registered = published
        .snapshot
        .projects
        .iter()
        .any(|entry| Uuid::parse_str(&entry.id).is_ok_and(|id| id == project_id));
    if !registered {
        return not_found();
    }
    match tokio::task::spawn_blocking(move || {
        files::read_artifact(&state_dir, &project, &relative)
    })
    .await
    {
        Ok(Ok(artifact)) => {
            let content_type = artifact.content_type;
            let content_disposition = artifact.content_disposition;
            let mut response = response_with(
                StatusCode::OK,
                content_type,
                full_body(Bytes::from(artifact.bytes)),
            );
            response.headers_mut().insert(
                CONTENT_DISPOSITION,
                HeaderValue::from_static(content_disposition),
            );
            response.headers_mut().insert(
                "x-content-type-options",
                HeaderValue::from_static("nosniff"),
            );
            response
        }
        Ok(Err(_)) | Err(_) => not_found(),
    }
}

enum CommandPolicy {
    Allowed,
    /// Import staging is wired only when the isolated state directory exists;
    /// until then the seam stays closed instead of forwarding a raw path.
    Import,
    ImportDeferred,
    Disabled,
}

fn command_policy(command: &UiCommand, mode: &Mode, has_state_dir: bool) -> CommandPolicy {
    match command {
        UiCommand::ProjectCreate { .. }
        | UiCommand::ProjectConfirm { .. }
        | UiCommand::ProjectRefresh { .. }
        | UiCommand::StartLocal => CommandPolicy::Allowed,
        UiCommand::ProjectImport { .. } => {
            if has_state_dir {
                CommandPolicy::Import
            } else {
                CommandPolicy::ImportDeferred
            }
        }
        UiCommand::Dispatch { .. }
        | UiCommand::Status { .. }
        | UiCommand::Approvals { .. }
        | UiCommand::Decide { .. }
        | UiCommand::Report { .. }
        | UiCommand::Retry { .. }
        | UiCommand::Cancel { .. }
            if *mode == Mode::Local =>
        {
            CommandPolicy::Allowed
        }
        _ => CommandPolicy::Disabled,
    }
}

async fn submit(
    client: Arc<BridgeClient>,
    command: UiCommand,
) -> Result<u64, BridgeSubmitError> {
    let pending = client.enqueue(command)?;
    // Wait for core acceptance outside the bridge queue lock and outside the
    // owner loop. A dropped waiter never retracts queued work.
    match tokio::task::spawn_blocking(move || pending.wait()).await {
        Ok(result) => result,
        Err(_) => Err(BridgeSubmitError::AcceptanceUnknown),
    }
}

/// Validate and copy the incoming document, then rewrite the core command to
/// the private staging path. Every failure here happens before acceptance.
async fn stage_import_command(
    app: Arc<HttpApp>,
    command: UiCommand,
) -> Result<(UiCommand, ImportStage), Response<ResponseBody>> {
    let Some(state_dir) = app.state_dir.clone() else {
        return Err(import_deferred());
    };
    let UiCommand::ProjectImport { project_id, path } = command else {
        return Err(bad_request());
    };
    let Some(filename) = path.to_str() else {
        return Err(invalid_import());
    };
    let filename = filename.to_owned();
    match tokio::task::spawn_blocking(move || files::stage_import(&state_dir, &filename)).await {
        Ok(Ok(stage)) => {
            let rewritten = UiCommand::ProjectImport {
                project_id,
                path: stage.core_path().to_path_buf(),
            };
            Ok((rewritten, stage))
        }
        Ok(Err(ImportError::InvalidInput)) => Err(invalid_import()),
        Ok(Err(ImportError::Internal)) | Err(_) => Err(internal()),
    }
}

async fn submit_import(
    client: Arc<BridgeClient>,
    command: UiCommand,
    stage: ImportStage,
) -> Result<u64, BridgeSubmitError> {
    let pending = client.enqueue_import(command, stage)?;
    match tokio::task::spawn_blocking(move || pending.wait()).await {
        Ok(result) => result,
        Err(_) => Err(BridgeSubmitError::AcceptanceUnknown),
    }
}

enum BodyReadError {
    TooLarge,
    Read,
}

async fn read_limited(mut body: Incoming) -> Result<Vec<u8>, BodyReadError> {
    let mut data = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| BodyReadError::Read)?;
        if let Some(chunk) = frame.data_ref() {
            if data.len().saturating_add(chunk.len()) > BODY_LIMIT {
                return Err(BodyReadError::TooLarge);
            }
            data.extend_from_slice(chunk);
        }
    }
    Ok(data)
}

fn events(request: &Request<Incoming>, app: &HttpApp) -> Response<ResponseBody> {
    if request.method() != hyper::Method::GET {
        return method_not_allowed();
    }
    // A stream opened after shutdown is refused; open streams are ended by
    // `stream_events` observing the same flag.
    if app.events.is_closed() {
        return unavailable();
    }
    let cursor = match cursor_from_request(request) {
        Ok(cursor) => cursor,
        Err(_) => return bad_request(),
    };
    let events = app.events.clone();
    let (initial, after) = match events.subscribe(cursor.clone()) {
        Ok(SubscribeResult::Checkpoint(checkpoint)) => {
            let after = checkpoint.frame.id.map_or(0, |id| id.seq);
            (vec![checkpoint.frame], after)
        }
        Ok(SubscribeResult::Replay(frames)) => {
            let requested = cursor.as_ref().map_or(0, |cursor| cursor.after);
            let after = frames
                .last()
                .and_then(|frame| frame.id)
                .map_or(requested, |id| id.seq);
            (frames, after)
        }
        Ok(SubscribeResult::GapThenCheckpoint { gap, checkpoint }) => {
            let after = checkpoint.id.map_or(0, |id| id.seq);
            (vec![gap, checkpoint], after)
        }
        Err(_) => return bad_request(),
    };
    let instance_id = events.instance_id();
    let changes = events.changes();
    let heartbeat = app.heartbeat;
    let cancelled = Arc::new(AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel(16);
    tokio::spawn(stream_events(
        events,
        changes,
        sender,
        cancelled.clone(),
        initial,
        after,
        instance_id,
        heartbeat,
    ));
    sse_response(SseBody { receiver, cancelled })
}

fn cursor_from_request(request: &Request<Incoming>) -> Result<Option<Cursor>, CursorError> {
    let mut instance = Vec::new();
    let mut after = Vec::new();
    if let Some(query) = request.uri().query() {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            match key {
                "instance_id" => instance.push(value),
                "after" => after.push(value),
                _ => {}
            }
        }
    }
    let mut last_event_id = Vec::new();
    for value in request.headers().get_all("last-event-id").iter() {
        last_event_id.push(value.to_str().map_err(|_| CursorError::Invalid {
            detail: "last event id header is invalid".into(),
        })?);
    }
    Cursor::from_query(&instance, &after, &last_event_id)
}

async fn stream_events(
    events: Arc<EventHubAccess>,
    mut changes: watch::Receiver<u64>,
    sender: mpsc::Sender<Bytes>,
    cancelled: Arc<AtomicBool>,
    initial: Vec<EventFrame>,
    mut after: u64,
    instance_id: Uuid,
    heartbeat: Duration,
) {
    for frame in initial {
        after = frame.id.map_or(after, |id| id.seq);
        if sender.send(frame_bytes(&frame)).await.is_err() {
            return;
        }
    }
    let mut last_emit = Instant::now();
    loop {
        if cancelled.load(Ordering::SeqCst) || sender.is_closed() {
            return;
        }
        loop {
            let next = events.subscribe(Some(Cursor::after(instance_id, after)));
            match next {
                Ok(SubscribeResult::Replay(frames)) if !frames.is_empty() => {
                    for frame in frames {
                        after = frame.id.map_or(after, |id| id.seq);
                        if sender.send(frame_bytes(&frame)).await.is_err() {
                            return;
                        }
                    }
                    last_emit = Instant::now();
                }
                Ok(SubscribeResult::Checkpoint(checkpoint)) => {
                    after = checkpoint.frame.id.map_or(after, |id| id.seq);
                    if sender.send(frame_bytes(&checkpoint.frame)).await.is_err() {
                        return;
                    }
                    last_emit = Instant::now();
                }
                Ok(SubscribeResult::GapThenCheckpoint { gap, checkpoint }) => {
                    if sender.send(frame_bytes(&gap)).await.is_err() {
                        return;
                    }
                    after = checkpoint.id.map_or(after, |id| id.seq);
                    if sender.send(frame_bytes(&checkpoint)).await.is_err() {
                        return;
                    }
                    last_emit = Instant::now();
                }
                Ok(SubscribeResult::Replay(_)) => break,
                // The cursor is produced by this loop, so a future/evicted
                // error cannot be repaired here without inventing events.
                Err(_) => break,
            }
        }
        // Shutdown ends the stream only after everything the owner already
        // published (real receipts, stopped snapshot) was handed to this
        // connection. Nothing is fabricated to fill the gap.
        if events.is_closed() {
            return;
        }
        // Token-free wait: a published change wakes through the watch channel;
        // otherwise a short poll notices a dropped reader promptly and a
        // heartbeat is emitted only after the configured idle interval.
        let remaining = heartbeat.saturating_sub(last_emit.elapsed());
        let wait = remaining.min(CANCEL_POLL).max(Duration::from_millis(1));
        match tokio::time::timeout(wait, changes.changed()).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return,
            Err(_) => {
                if last_emit.elapsed() >= heartbeat {
                    if sender.send(Bytes::from_static(EventHub::heartbeat())).await.is_err() {
                        return;
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }
}

const CANCEL_POLL: Duration = Duration::from_millis(250);

fn frame_bytes(frame: &EventFrame) -> Bytes {
    Bytes::copy_from_slice(frame.encoded())
}

struct SseBody {
    receiver: mpsc::Receiver<Bytes>,
    cancelled: Arc<AtomicBool>,
}

impl Body for SseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.receiver.poll_recv(context) {
            Poll::Ready(Some(data)) => Poll::Ready(Some(Ok(Frame::data(data)))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for SseBody {
    fn drop(&mut self) {
        // Closing the HTTP reader is not business cancellation; the producer
        // only stops writing frames for this connection.
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

fn json_content_type(value: Option<&HeaderValue>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let mut parts = value.split(';');
    let Some(media) = parts.next() else {
        return false;
    };
    if !media.trim().eq_ignore_ascii_case("application/json") {
        return false;
    }
    parts.all(|parameter| {
        let Some((name, value)) = parameter.split_once('=') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("charset")
            && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
    })
}

fn json_response(status: StatusCode, value: &Value) -> Response<ResponseBody> {
    match serde_json::to_vec(value) {
        Ok(bytes) => response_with(status, "application/json", full_body(Bytes::from(bytes))),
        Err(_) => response_with(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application/json",
            full_body(Bytes::from_static(MALFORMED_BODY.as_bytes())),
        ),
    }
}

fn response_with(
    status: StatusCode,
    content_type: &'static str,
    body: ResponseBody,
) -> Response<ResponseBody> {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn full_body(bytes: Bytes) -> ResponseBody {
    Full::new(bytes).map_err(|never| match never {}).boxed_unsync()
}

fn sse_response(body: SseBody) -> Response<ResponseBody> {
    response_with(StatusCode::OK, "text/event-stream", body.boxed_unsync())
}

fn error_response(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    next_step: &'static str,
) -> Response<ResponseBody> {
    json_response(status, &json!({"error": {"code": code, "message": message, "next_step": next_step}}))
}

fn bad_request() -> Response<ResponseBody> {
    error_response(
        StatusCode::BAD_REQUEST,
        "bad_request",
        "请求格式无效；未执行任何操作",
        "核对方法、JSON 形状、命令 kind 与参数类型后重试",
    )
}

fn not_found() -> Response<ResponseBody> {
    error_response(
        StatusCode::NOT_FOUND,
        "not_found",
        "接口不存在；未执行任何操作",
        "核对 v1 路由后重试",
    )
}

fn method_not_allowed() -> Response<ResponseBody> {
    error_response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "请求方法不受支持；未执行任何操作",
        "使用该路由允许的 HTTP 方法重试",
    )
}

fn unsupported_media_type() -> Response<ResponseBody> {
    error_response(
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "unsupported_media_type",
        "请求 Content-Type 不受支持；未读取命令",
        "使用 application/json（可带 charset=utf-8）重试",
    )
}

fn payload_too_large() -> Response<ResponseBody> {
    error_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
        "请求体超过 1 MiB 上限；未执行任何操作",
        "缩小请求体后重试",
    )
}

fn command_disabled() -> Response<ResponseBody> {
    error_response(
        StatusCode::FORBIDDEN,
        "command_disabled",
        "该命令在隔离验收配置中未开放；未入队、未执行",
        "使用允许的命令列表或在显式 --local 下重试",
    )
}

fn busy() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "busy",
        "桥接队列已满；该命令未被受理",
        "等待现有请求完成后再提交，不要自动重投",
    )
}

fn unavailable() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "后台服务暂不可用；该命令未被受理",
        "重新启动隔离 daemon 后重试",
    )
}

fn acceptance_unknown() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "acceptance_unknown",
        "无法确认命令是否被受理；不要自动重试",
        "以新请求重新提交前先核对状态",
    )
}

fn invalid_settings() -> Response<ResponseBody> {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_settings",
        "连接设置不符合 schema 2 或校验失败；未保存",
        "补全模型设置与四角色 Worker 后重试",
    )
}

fn internal() -> Response<ResponseBody> {
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "服务内部错误；结果未确认",
        "检查固定诊断信息后重试，不要自动重投已受理请求",
    )
}

fn invalid_import() -> Response<ResponseBody> {
    error_response(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_import",
        "导入文件不符合隔离导入规则；未受理",
        "把 ASCII .md/.txt 文档放入隔离 imports/ 目录并只传文件名后重试",
    )
}

fn import_deferred() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "隔离状态目录尚未就绪；导入未受理",
        "启动隔离 daemon 后重试",
    )
}

fn route_deferred() -> Response<ResponseBody> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "该接口尚未在本次交付中开放；未执行任何操作",
        "等待后续版本开放后重试",
    )
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}
