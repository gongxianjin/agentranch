use crate::{
    api::contract::CommandResponse,
    core::service::AppSnapshot,
};
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Value};
use std::{collections::VecDeque, fmt, sync::Arc};
use uuid::Uuid;

pub const MAX_EVENT_FRAMES: usize = 1024;
pub const MAX_EVENT_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventId {
    pub instance_id: Uuid,
    pub seq: u64,
}

impl EventId {
    pub fn parse(value: &str) -> Result<Self, CursorError> {
        let (instance, seq) = value.split_once(':').ok_or_else(|| CursorError::Invalid { detail: "event id must be instance:sequence".into() })?;
        if instance.is_empty() || seq.is_empty() || seq.contains(':') {
            return Err(CursorError::Invalid { detail: "event id format is invalid".into() });
        }
        let instance_id = Uuid::parse_str(instance).map_err(|_| CursorError::Invalid { detail: "event id instance is invalid".into() })?;
        let seq = seq.parse::<u64>().map_err(|_| CursorError::Invalid { detail: "event id sequence is invalid".into() })?;
        Ok(Self { instance_id, seq })
    }
}

impl fmt::Display for EventId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}", self.instance_id, self.seq)
    }
}

impl Serialize for EventId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: Serializer {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EventId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: Deserializer<'de> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub instance_id: Option<Uuid>,
    pub after: u64,
}

impl Cursor {
    pub fn current(after: u64) -> Self { Self { instance_id: None, after } }
    pub fn after(instance_id: Uuid, after: u64) -> Self { Self { instance_id: Some(instance_id), after } }

    pub fn parse(value: &str) -> Result<Self, CursorError> {
        let id = EventId::parse(value)?;
        Ok(Self { instance_id: Some(id.instance_id), after: id.seq })
    }

    pub fn from_query(instance: &[&str], after: &[&str], last_event_id: &[&str]) -> Result<Option<Self>, CursorError> {
        if instance.len() > 1 || after.len() > 1 || last_event_id.len() > 1 {
            return Err(CursorError::Invalid { detail: "cursor parameter is duplicated".into() });
        }
        if !last_event_id.is_empty() && (!instance.is_empty() || !after.is_empty()) {
            return Err(CursorError::Invalid { detail: "cursor forms cannot be combined".into() });
        }
        if let Some(value) = last_event_id.first() {
            return Self::parse(value).map(Some);
        }
        if instance.is_empty() && after.is_empty() {
            return Ok(None);
        }
        let after = after.first().ok_or_else(|| CursorError::Invalid { detail: "after is required".into() })?
            .parse::<u64>().map_err(|_| CursorError::Invalid { detail: "after is not a decimal sequence".into() })?;
        let instance_id = match instance.first() {
            Some(value) => Some(Uuid::parse_str(value).map_err(|_| CursorError::Invalid { detail: "instance is invalid".into() })?),
            None => None,
        };
        if instance_id.is_none() && after != 0 {
            return Err(CursorError::Invalid { detail: "instance may be omitted only for after=0".into() });
        }
        Ok(Some(Self { instance_id, after }))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CursorError {
    Invalid { detail: String },
    Future { after: u64, head: u64 },
}

impl fmt::Display for CursorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { detail } => write!(formatter, "invalid cursor: {detail}"),
            Self::Future { after, head } => write!(formatter, "cursor {after} is ahead of head {head}"),
        }
    }
}

impl std::error::Error for CursorError {}

#[derive(Clone, Debug)]
pub enum PublishedEvent {
    LiveOutput(Value),
    CommandResult(CommandResponse),
    Snapshot(AppSnapshot),
    EventUnavailable(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct EventFrame {
    pub id: Option<EventId>,
    pub event: String,
    pub data: Value,
    pub encoded_len: usize,
    /// Canonical SSE wire bytes. Cloned frames share this buffer so the
    /// transport never re-encodes a frame into a different representation.
    encoded: Arc<[u8]>,
}

impl EventFrame {
    fn new(id: Option<EventId>, event: impl Into<String>, data: Value) -> Self {
        let event = event.into();
        let encoded: Arc<[u8]> = EncodedFrame::new(id.as_ref(), &event, &data).bytes.into();
        let encoded_len = encoded.len();
        Self { id, event, data, encoded_len, encoded }
    }

    pub fn encoded(&self) -> &[u8] { &self.encoded }
}

#[derive(Clone, Debug)]
pub struct PublishedView {
    pub instance_id: Uuid,
    pub head: u64,
    pub snapshot_seq: u64,
    pub snapshot: Arc<AppSnapshot>,
}

#[derive(Clone, Debug)]
pub struct Checkpoint {
    pub frame: EventFrame,
    pub view: PublishedView,
}

#[derive(Clone, Debug)]
pub enum SubscribeResult {
    Checkpoint(Checkpoint),
    Replay(Vec<EventFrame>),
    GapThenCheckpoint { gap: EventFrame, checkpoint: EventFrame },
}

pub struct EventHub {
    instance_id: Uuid,
    head: u64,
    snapshot_seq: u64,
    snapshot: Arc<AppSnapshot>,
    ring: VecDeque<EventFrame>,
    ring_bytes: usize,
    change_notifier: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl EventHub {
    pub fn new(snapshot: Arc<AppSnapshot>) -> Self {
        Self {
            instance_id: Uuid::new_v4(),
            head: 0,
            snapshot_seq: 0,
            snapshot,
            ring: VecDeque::new(),
            ring_bytes: 0,
            change_notifier: None,
        }
    }

    /// Registers the transport wake callback. The reader still re-reads the
    /// window after every wakeup, so a notification racing with a read cannot
    /// hide a published frame.
    pub fn set_change_notifier(&mut self, notifier: Arc<dyn Fn() + Send + Sync>) {
        self.change_notifier = Some(notifier);
    }

    pub fn instance_id(&self) -> Uuid { self.instance_id }
    pub fn head(&self) -> u64 { self.head }
    pub fn len(&self) -> usize { self.ring.len() }
    pub fn oldest_seq(&self) -> u64 { self.ring.front().and_then(|frame| frame.id).map_or(self.head.saturating_add(1), |id| id.seq) }

    pub fn publish(&mut self, event: PublishedEvent) -> EventId {
        self.head = self.head.saturating_add(1);
        let id = EventId { instance_id: self.instance_id, seq: self.head };
        let (frame, snapshot) = match event {
            PublishedEvent::Snapshot(snapshot) => {
                let snapshot = Arc::new(snapshot);
                (snapshot_frame(Some(id), &snapshot), Some(snapshot))
            }
            event => {
                let (name, data, fallback) = event_data(event);
                (bounded_frame(Some(id), name, data, fallback), None)
            }
        };
        if let Some(snapshot) = snapshot {
            self.snapshot = snapshot;
            self.snapshot_seq = id.seq;
        }
        self.ring_bytes = self.ring_bytes.saturating_add(frame.encoded_len);
        self.ring.push_back(frame);
        while self.ring.len() > MAX_EVENT_FRAMES || self.ring_bytes > MAX_EVENT_BYTES {
            if let Some(old) = self.ring.pop_front() { self.ring_bytes = self.ring_bytes.saturating_sub(old.encoded_len); }
        }
        if let Some(notifier) = &self.change_notifier { notifier(); }
        id
    }

    pub fn checkpoint(&self) -> Checkpoint {
        let view = self.published_view();
        let id = EventId { instance_id: self.instance_id, seq: self.head };
        Checkpoint { frame: snapshot_frame(Some(id), &view.snapshot), view }
    }

    pub fn published_view(&self) -> PublishedView {
        PublishedView { instance_id: self.instance_id, head: self.head, snapshot_seq: self.snapshot_seq, snapshot: self.snapshot.clone() }
    }

    pub fn subscribe(&self, cursor: Option<Cursor>) -> Result<SubscribeResult, CursorError> {
        let Some(cursor) = cursor else { return Ok(SubscribeResult::Checkpoint(self.checkpoint())); };
        if let Some(instance_id) = cursor.instance_id {
            if instance_id != self.instance_id { return Ok(self.gap_checkpoint("instance_changed", Some(instance_id), cursor.after)); }
        }
        if cursor.after > self.head { return Err(CursorError::Future { after: cursor.after, head: self.head }); }
        if cursor.after < self.oldest_seq().saturating_sub(1) {
            return Ok(self.gap_checkpoint("history_evicted", cursor.instance_id, cursor.after));
        }
        Ok(SubscribeResult::Replay(self.ring.iter().filter(|frame| frame.id.is_some_and(|id| id.seq > cursor.after)).cloned().collect()))
    }

    /// Idle SSE heartbeat: an unnamed comment line, per spec §4. It carries
    /// no `id`, `event` or `data`, so it cannot advance a cursor or be
    /// mistaken for a published event. A `&'static [u8]` keeps the event
    /// module free of the daemon-only `bytes` dependency; the transport wraps
    /// it without copying.
    pub fn heartbeat() -> &'static [u8] { b": keep-alive\n\n" }

    fn gap_checkpoint(&self, reason: &str, requested_instance_id: Option<Uuid>, after: u64) -> SubscribeResult {
        let gap = EventFrame::new(None, "gap", json!({
            "reason": reason,
            "requested_instance_id": requested_instance_id.map(|id| id.to_string()),
            "after": after,
            "oldest": self.oldest_seq(),
            "head": self.head,
            "instance_id": self.instance_id.to_string(),
        }));
        SubscribeResult::GapThenCheckpoint { gap, checkpoint: self.checkpoint().frame }
    }
}

fn event_data(event: PublishedEvent) -> (String, Value, Value) {
    match event {
        PublishedEvent::LiveOutput(value) => (
            "live_output".into(),
            value,
            json!({ "kind": "live_output", "reason": "frame_too_large" }),
        ),
        PublishedEvent::CommandResult(response) => {
            let request_id = command_request_id(&response);
            (
                "command_result".into(),
                serde_json::to_value(response).unwrap_or(Value::Null),
                json!({ "kind": "command_result", "request_id": request_id, "reason": "frame_too_large" }),
            )
        }
        PublishedEvent::Snapshot(snapshot) => {
            let snapshot = Arc::new(snapshot);
            (
                "snapshot".into(),
                snapshot_value(&snapshot),
                json!({ "kind": "snapshot", "reason": "frame_too_large" }),
            )
        }
        PublishedEvent::EventUnavailable(value) => (
            "event_unavailable".into(),
            value,
            json!({ "kind": "event_unavailable", "reason": "frame_too_large" }),
        ),
    }
}

fn command_request_id(response: &CommandResponse) -> u64 {
    match response {
        CommandResponse::Ok { request_id, .. } | CommandResponse::Err { request_id, .. } => *request_id,
    }
}

fn snapshot_frame(id: Option<EventId>, snapshot: &Arc<AppSnapshot>) -> EventFrame {
    bounded_frame(
        id,
        "snapshot",
        snapshot_value(snapshot),
        json!({ "kind": "snapshot", "reason": "frame_too_large" }),
    )
}

fn bounded_frame(id: Option<EventId>, event: impl Into<String>, data: Value, fallback: Value) -> EventFrame {
    let candidate = EventFrame::new(id, event, data);
    if candidate.encoded_len > MAX_EVENT_BYTES {
        EventFrame::new(candidate.id, "event_unavailable", fallback)
    } else {
        candidate
    }
}

fn snapshot_value(snapshot: &Arc<AppSnapshot>) -> Value { serde_json::to_value(snapshot.as_ref()).unwrap_or(Value::Null) }

struct EncodedFrame {
    bytes: Vec<u8>,
}

impl EncodedFrame {
    fn new(id: Option<&EventId>, event: &str, data: &Value) -> Self {
        let mut bytes = Vec::new();
        if let Some(id) = id {
            append_line(&mut bytes, "id", &id.to_string());
        }
        append_line(&mut bytes, "event", event);
        let data = serde_json::to_string(data).unwrap_or_else(|_| "null".into());
        append_line(&mut bytes, "data", &data);
        bytes.extend_from_slice(b"\n");
        Self { bytes }
    }
}

fn append_line(bytes: &mut Vec<u8>, field: &str, value: &str) {
    bytes.extend_from_slice(field.as_bytes());
    bytes.extend_from_slice(b": ");
    bytes.extend_from_slice(value.as_bytes());
    bytes.extend_from_slice(b"\n");
}
