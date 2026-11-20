use agentranch::core::{live_output::LiveOutput, model_stream::read_chat_stream};
use std::io::{self, Read};

// Fails if the implementation buffers to EOF, loses UTF-8 at read boundaries,
// or treats an unfinished/length-limited response as a complete artifact.
struct BytesOneAtATime { bytes: Vec<u8>, at: usize }
impl Read for BytesOneAtATime {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.bytes.len() { return Ok(0); }
        out[0] = self.bytes[self.at]; self.at += 1; Ok(1)
    }
}
#[test]
fn split_unicode_crlf_multiline_data_and_role_only_events_preserve_actual_text() {
    let raw = concat!(": comment\r\n\r\n", "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\r\n\r\n", "data: {\"choices\":[\n", "data: {\"delta\":{\"content\":\"成都\\n<html>\"}}]}\n\n", "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n", "data: [DONE]\n\n");
    let mut events = vec![];
    let result = read_chat_stream(BytesOneAtATime { bytes: raw.as_bytes().to_vec(), at: 0 }, &mut |event| events.push(event)).unwrap();
    assert_eq!(result.content, "成都\n<html>");
    assert_eq!(events, vec![LiveOutput::Connected, LiveOutput::Delta("成都\n<html>".into()), LiveOutput::ModelFinished]);
}
#[test]
fn delta_is_emitted_before_the_reader_can_supply_the_next_event() {
    struct Gated { reads: usize, seen: std::sync::Arc<std::sync::atomic::AtomicBool> }
    impl Read for Gated {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let data = match self.reads {
                0 => b"data: {\"choices\":[{\"delta\":{\"content\":\"first\"}}]}\n\n".as_slice(),
                1 => { assert!(self.seen.load(std::sync::atomic::Ordering::SeqCst), "first delta was buffered"); b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n" },
                _ => return Ok(0),
            };
            self.reads += 1; out[..data.len()].copy_from_slice(data); Ok(data.len())
        }
    }
    let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result = read_chat_stream(Gated { reads: 0, seen: seen.clone() }, &mut |e| if e == LiveOutput::Delta("first".into()) { seen.store(true, std::sync::atomic::Ordering::SeqCst); }).unwrap();
    assert_eq!(result.content, "first");
}
#[test]
fn incomplete_error_and_nonstop_streams_fail_without_model_finished() {
    for raw in ["data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n", "data: {\"error\":{\"message\":\"offline\"}}\n\n", "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n", "data: [DONE]\n\n"] {
        let mut events = vec![];
        assert!(read_chat_stream(raw.as_bytes(), &mut |e| events.push(e)).is_err());
        assert!(!events.contains(&LiveOutput::ModelFinished));
    }
}

// Round 3 (dist/live-real-20261120-112004): a reasoning model streams large
// reasoning frames before any content. The raw SSE byte bound must not kill
// the stream, and reasoning deltas must surface as live output while the
// final content stays content-only.
#[test]
fn reasoning_deltas_are_forwarded_as_live_output_but_never_join_content() {
    let raw = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking hard \"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning\":{\"text\":\"about design\"}}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"<html>final</html>\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut events = vec![];
    let result = read_chat_stream(raw.as_bytes(), &mut |event| events.push(event)).unwrap();
    assert_eq!(result.content, "<html>final</html>");
    assert_eq!(events, vec![
        LiveOutput::Connected,
        LiveOutput::Delta("thinking hard ".into()),
        LiveOutput::Delta("about design".into()),
        LiveOutput::Delta("<html>final</html>".into()),
        LiveOutput::ModelFinished,
    ]);
}
#[test]
fn raw_stream_bound_is_sixteen_mib_not_one() {
    // 1MiB of reasoning JSON frames must stream through cleanly; the bound
    // that failed run b242a25d at 58 s was the raw-byte counter.
    let frame = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"0123456789abcdef\"}}]}\n\n";
    let frames_per_mib = (1 << 20) / frame.len() + 2;
    let mut raw = String::with_capacity(frames_per_mib * frame.len());
    for _ in 0..frames_per_mib { raw.push_str(frame); }
    raw.push_str("data: {\"choices\":[{\"delta\":{\"content\":\"end\"}}]}\n\n");
    raw.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
    raw.push_str("data: [DONE]\n\n");
    let mut reasoning_seen = false;
    let result = read_chat_stream(raw.as_bytes(), &mut |event| {
        if let LiveOutput::Delta(text) = event { if text.ends_with("end") { reasoning_seen = true; } }
    }).unwrap();
    assert_eq!(result.content, "end");
    assert!(reasoning_seen, "content delta must arrive after 1MiB of reasoning frames");
}
