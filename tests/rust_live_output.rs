use agentranch::core::live_output::{LiveOutput, LiveLines, capture_with_live, write_live};

#[test]
fn line_decoder_handles_split_unicode_and_keeps_diagnostics() {
    let raw = "warning\nAO-LIVE {\"kind\":\"delta\",\"text\":\"成都\\n页面\"}\nend";
    let mut decoder = LiveLines::default(); let mut events = vec![]; let mut diagnostics = vec![];
    for byte in raw.as_bytes() {
        decoder.push(&[*byte], &mut |e| events.push(e), &mut |b| diagnostics.extend_from_slice(b)).unwrap();
    }
    decoder.finish(&mut |b| diagnostics.extend_from_slice(b)).unwrap();
    assert_eq!(events, vec![LiveOutput::Delta("成都\n页面".into())]);
    assert_eq!(diagnostics, b"warning\nend");
}
#[test]
fn owner_tee_preserves_raw_prefix_count_and_eof() {
    let mut wire = vec![];
    write_live(&mut wire, &LiveOutput::Delta("成都".repeat(900))).unwrap();
    wire.extend_from_slice(b"ordinary diagnostic\n");
    let mut forwarded = vec![];
    let capture = capture_with_live(wire.as_slice(), &mut |line| forwarded.extend_from_slice(line));
    assert!(capture.eof && capture.error.is_none());
    assert_eq!(capture.total_read_bytes, wire.len() as u64);
    assert_eq!(capture.prefix, wire[..4096]);
    assert!(forwarded.starts_with(b"AO-LIVE "));
    assert!(!String::from_utf8(forwarded).unwrap().contains("ordinary diagnostic"));
}
#[test]
fn ordinary_diagnostics_stream_without_waiting_for_newline_or_using_live_line_budget() {
    let mut lines = LiveLines::default();
    let mut diagnostics = vec![];
    lines.push(b"ordinary diagnostic", &mut |_| panic!("no live event"), &mut |b| diagnostics.extend_from_slice(b)).unwrap();
    assert_eq!(diagnostics, b"ordinary diagnostic");
    let huge = vec![b'e'; 128 * 1024];
    lines.push(&huge, &mut |_| panic!("no live event"), &mut |b| diagnostics.extend_from_slice(b)).unwrap();
    lines.finish(&mut |b| diagnostics.extend_from_slice(b)).unwrap();
    assert_eq!(diagnostics.len(), huge.len() + b"ordinary diagnostic".len());
}
#[test]
fn malformed_and_unterminated_live_lines_are_not_silently_accepted() {
    for bytes in [b"AO-LIVE not-json\n".as_slice(), b"AO-LIVE {\"kind\":\"delta\",\"text\":\"partial\"}"] {
        let mut lines = LiveLines::default();
        let push = lines.push(bytes, &mut |_| {}, &mut |_| {});
        assert!(push.is_err() || lines.finish(&mut |_| {}).is_err());
    }
}

#[test]
fn oversized_live_line_is_retained_as_bounded_diagnostic() {
    let mut bytes = b"AO-LIVE ".to_vec();
    bytes.extend(std::iter::repeat_n(b'x', 64 * 1024));
    let mut lines = LiveLines::default();
    let mut diagnostics = Vec::new();
    let result = lines.push(&bytes, &mut |_| {}, &mut |chunk| diagnostics.extend_from_slice(chunk));
    assert!(result.is_err());
    assert_eq!(diagnostics, bytes);
}
