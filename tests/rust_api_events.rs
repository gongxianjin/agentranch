use agentranch::{
    api::{contract::CommandResponse, events::{Cursor, CursorError, EventHub, EventId, EventFrame, PublishedEvent, SubscribeResult, MAX_EVENT_FRAMES, MAX_EVENT_BYTES}},
    config::Mode,
    core::service::{AppSnapshot, Readiness, ServiceError},
};
use serde_json::json;
use std::sync::Arc;
use uuid::Uuid;

fn snapshot() -> AppSnapshot {
    AppSnapshot {
        mode: Mode::Real,
        role: "test".into(),
        readiness: Readiness::NotReady,
        actor: None,
        observed_at: 0,
        running: true,
        dependencies: vec![],
        runs: vec![],
        nodes: vec![],
        approvals: vec![],
        errors: vec![],
        unfinished: 0,
        projects: vec![],
        gateway: None,
        provision_ledger: None,
    }
}

fn replayed_frame(hub: &EventHub, seq: u64) -> EventFrame {
    let frames = match hub.subscribe(Some(Cursor::current(0))).unwrap() {
        SubscribeResult::Replay(frames) => frames,
        other => panic!("expected replay, got {other:?}"),
    };
    frames.into_iter().find(|frame| frame.id.is_some_and(|id| id.seq == seq)).unwrap()
}

#[test]
fn event_ids_start_at_one_and_serialize_with_instance() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    let id = hub.publish(PublishedEvent::LiveOutput(json!({"text":"one"})));
    assert_eq!(id.seq, 1);
    assert_eq!(id.instance_id, hub.instance_id());
    assert_eq!(id.to_string(), format!("{}:1", hub.instance_id()));
    assert_eq!(id, EventId::parse(&id.to_string()).unwrap());
}

#[test]
fn no_cursor_is_checkpoint_but_after_zero_replays_events() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    hub.publish(PublishedEvent::LiveOutput(json!({"n":1})));
    let checkpoint = match hub.subscribe(None).unwrap() {
        SubscribeResult::Checkpoint(checkpoint) => checkpoint,
        other => panic!("expected checkpoint, got {other:?}"),
    };
    assert_eq!(checkpoint.frame.id.unwrap().seq, 1);
    assert_eq!(checkpoint.view.head, hub.head());
    let replay = match hub.subscribe(Some(Cursor::current(0))).unwrap() {
        SubscribeResult::Replay(value) => value,
        other => panic!("expected replay, got {other:?}"),
    };
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].id.unwrap().seq, 1);
}

#[test]
fn cursor_boundaries_and_errors_are_deterministic() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    hub.publish(PublishedEvent::LiveOutput(json!({"n":1})));
    let head = hub.head();
    assert!(matches!(hub.subscribe(Some(Cursor::current(head))).unwrap(), SubscribeResult::Replay(frames) if frames.is_empty()));
    assert!(matches!(hub.subscribe(Some(Cursor::current(head + 1))), Err(CursorError::Future { .. })));
    let other = Uuid::new_v4();
    assert!(matches!(hub.subscribe(Some(Cursor::after(other, 0))).unwrap(), SubscribeResult::GapThenCheckpoint { .. }));
    assert!(matches!(Cursor::parse("not-an-event-id"), Err(CursorError::Invalid { .. })));
    assert!(matches!(Cursor::parse(&format!("{}:x", hub.instance_id())), Err(CursorError::Invalid { .. })));
}

#[test]
fn eviction_reports_gap_and_keeps_1024_frames() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    for n in 0..(MAX_EVENT_FRAMES + 3) {
        hub.publish(PublishedEvent::LiveOutput(json!({"n":n})));
    }
    assert_eq!(hub.len(), MAX_EVENT_FRAMES);
    assert_eq!(hub.oldest_seq(), 4);
    let result = hub.subscribe(Some(Cursor::current(0))).unwrap();
    match result {
        SubscribeResult::GapThenCheckpoint { gap, checkpoint } => {
            assert_eq!(gap.id, None);
            assert_eq!(gap.event, "gap");
            assert_eq!(checkpoint.id.unwrap().seq, hub.head());
        }
        other => panic!("expected eviction gap, got {other:?}"),
    }
}

#[test]
fn snapshot_publication_updates_view_but_later_result_does_not() {
    let initial = Arc::new(snapshot());
    let mut hub = EventHub::new(initial.clone());
    let snapshot_id = hub.publish(PublishedEvent::Snapshot((*initial).clone()));
    let view = hub.published_view();
    assert_eq!(view.snapshot_seq, snapshot_id.seq);
    assert_eq!(view.snapshot_seq, view.head);
    hub.publish(PublishedEvent::CommandResult(CommandResponse::Ok { request_id: 7, result: json!({"ok":true}) }));
    let later = hub.published_view();
    assert_eq!(later.snapshot_seq, snapshot_id.seq);
    assert_eq!(later.head, snapshot_id.seq + 1);
}

#[test]
fn oversized_event_becomes_unavailable_and_sequence_continues() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    let huge = "x".repeat(MAX_EVENT_BYTES);
    let id = hub.publish(PublishedEvent::LiveOutput(json!({"text": huge})));
    let next = hub.publish(PublishedEvent::LiveOutput(json!({"text":"small"})));
    assert_eq!(next.seq, id.seq + 1);
    let frames = match hub.subscribe(Some(Cursor::current(0))).unwrap() {
        SubscribeResult::Replay(frames) => frames,
        other => panic!("expected replay, got {other:?}"),
    };
    let frame = frames.iter().find(|frame| frame.id.is_some_and(|frame_id| frame_id.seq == id.seq)).unwrap();
    assert_eq!(frame.event, "event_unavailable");
    assert_eq!(frame.data["reason"], "frame_too_large");
}

#[test]
fn heartbeat_is_an_unnamed_comment_and_query_parser_rejects_duplicates() {
    let heartbeat = EventHub::heartbeat();
    assert_eq!(heartbeat, b": keep-alive\n\n");
    assert!(!heartbeat.windows(3).any(|window| window == b"id:"));
    assert!(!heartbeat.windows(6).any(|window| window == b"event:"));
    assert!(Cursor::from_query(&["instance"], &["0"], &[]).is_err());
    assert!(Cursor::from_query(&[], &["0", "1"], &[]).is_err());
    assert!(Cursor::from_query(&[], &[], &["bad-id"]).is_err());
}

#[test]
fn empty_hub_checkpoint_uses_head_zero_and_preserves_instance_id() {
    let hub = EventHub::new(Arc::new(snapshot()));
    let checkpoint = match hub.subscribe(None).unwrap() {
        SubscribeResult::Checkpoint(checkpoint) => checkpoint,
        other => panic!("expected checkpoint, got {other:?}"),
    };
    assert_eq!(hub.head(), 0);
    assert_eq!(checkpoint.frame.id, Some(EventId { instance_id: hub.instance_id(), seq: 0 }));
    assert_eq!(checkpoint.frame.event, "snapshot");
    assert_eq!(checkpoint.view.head, 0);
    assert_eq!(hub.checkpoint().frame.id, checkpoint.frame.id);
}

#[test]
fn checkpoint_falls_back_when_snapshot_exceeds_wire_bound_but_view_keeps_snapshot() {
    let mut huge = snapshot();
    huge.role = "x".repeat(MAX_EVENT_BYTES);
    let hub = EventHub::new(Arc::new(huge));
    let checkpoint = hub.checkpoint();
    assert_eq!(checkpoint.frame.id, Some(EventId { instance_id: hub.instance_id(), seq: 0 }));
    assert_eq!(checkpoint.frame.event, "event_unavailable");
    assert_eq!(checkpoint.frame.data["reason"], "frame_too_large");
    assert!(checkpoint.frame.encoded_len <= MAX_EVENT_BYTES);
    assert_eq!(checkpoint.view.snapshot.role.len(), MAX_EVENT_BYTES);
    assert_eq!(hub.published_view().snapshot.role.len(), MAX_EVENT_BYTES);
}

#[test]
fn oversized_command_results_preserve_request_id_in_unavailable_metadata() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    let result_id = hub.publish(PublishedEvent::CommandResult(CommandResponse::Ok {
        request_id: 41,
        result: json!({"text": "x".repeat(MAX_EVENT_BYTES)}),
    }));
    let error_id = hub.publish(PublishedEvent::CommandResult(CommandResponse::Err {
        request_id: 42,
        error: ServiceError {
            code: "too_large".into(),
            message: "x".repeat(MAX_EVENT_BYTES),
            next_step: "retry".into(),
        },
    }));
    for (id, request_id) in [(result_id, 41), (error_id, 42)] {
        let frame = replayed_frame(&hub, id.seq);
        assert_eq!(frame.event, "event_unavailable");
        assert_eq!(frame.data["request_id"], request_id);
        assert_eq!(frame.data["reason"], "frame_too_large");
        assert!(frame.encoded_len <= MAX_EVENT_BYTES);
    }
}

#[test]
fn exact_oldest_minus_one_cursor_is_accepted_without_gap() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    for n in 0..(MAX_EVENT_FRAMES + 3) {
        hub.publish(PublishedEvent::LiveOutput(json!({"n":n})));
    }
    let after_oldest = hub.oldest_seq().saturating_sub(1);
    match hub.subscribe(Some(Cursor::current(after_oldest))).unwrap() {
        SubscribeResult::Replay(frames) => assert_eq!(frames.first().unwrap().id.unwrap().seq, hub.oldest_seq()),
        other => panic!("expected replay at oldest-1, got {other:?}"),
    }
}

#[test]
fn different_instance_gap_precedes_future_sequence_error() {
    let hub = EventHub::new(Arc::new(snapshot()));
    let other = Uuid::new_v4();
    match hub.subscribe(Some(Cursor::after(other, u64::MAX))).unwrap() {
        SubscribeResult::GapThenCheckpoint { gap, checkpoint } => {
            assert_eq!(gap.event, "gap");
            assert_eq!(gap.data["reason"], "instance_changed");
            assert_eq!(checkpoint.event, "snapshot");
        }
        other => panic!("expected different-instance gap, got {other:?}"),
    }
}

#[test]
fn encoded_len_models_sse_frame_prefixes_and_delimiters() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    let id = hub.publish(PublishedEvent::LiveOutput(json!({"n":1})));
    let frame = replayed_frame(&hub, id.seq);
    let expected = format!(
        "id: {}\nevent: live_output\ndata: {}\n\n",
        id,
        json!({"n":1})
    );
    assert_eq!(frame.encoded_len, expected.len());
}

#[test]
fn byte_budget_eviction_precedes_frame_count_and_reports_boundary_gap() {
    let mut hub = EventHub::new(Arc::new(snapshot()));
    let payload = "x".repeat(64 * 1024);
    let mut published = Vec::new();
    while hub.head() < (MAX_EVENT_FRAMES as u64 - 1) && hub.len() < MAX_EVENT_FRAMES {
        published.push(hub.publish(PublishedEvent::LiveOutput(json!({"payload": payload}))));
        if hub.oldest_seq() > 1 { break; }
    }
    assert!(hub.head() < MAX_EVENT_FRAMES as u64);
    assert!(hub.len() < MAX_EVENT_FRAMES);
    assert!(hub.oldest_seq() > 1);
    let oldest_minus_one = hub.oldest_seq() - 1;
    match hub.subscribe(Some(Cursor::current(oldest_minus_one))).unwrap() {
        SubscribeResult::Replay(frames) => assert_eq!(frames.first().unwrap().id.unwrap().seq, hub.oldest_seq()),
        other => panic!("expected replay at byte boundary, got {other:?}"),
    }
    match hub.subscribe(Some(Cursor::current(oldest_minus_one - 1))).unwrap() {
        SubscribeResult::GapThenCheckpoint { gap, checkpoint } => {
            assert_eq!(gap.data["reason"], "history_evicted");
            assert_eq!(checkpoint.id, Some(EventId { instance_id: hub.instance_id(), seq: hub.head() }));
            assert_eq!(checkpoint.event, "snapshot");
        }
        other => panic!("expected pre-boundary gap, got {other:?}"),
    }
    assert!(published.len() < MAX_EVENT_FRAMES);
}
