//! Task 2: the shared model request path must enforce a total wall-clock
//! budget. Blocking reqwest's own `.timeout()` is a per-operation inactivity
//! bound (it recomputes `Instant::now() + duration` on every read), so a
//! server that keeps dribbling bytes can hold a call open indefinitely.
//! These tests pin the total-budget behavior over real loopback TCP.
use agentranch::core::model_request::{DeadlineStream, send_chat_request};
use agentranch::core::model_stream::read_chat_stream;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BUDGET: Duration = Duration::from_millis(300);

/// Accepts exactly one request, records its request line, and hands the socket
/// to `reply`. The request head and any Content-Length body are consumed first
/// so the reply cannot race the upload.
fn serve_once(
    listener: TcpListener,
    requests: Arc<Mutex<Vec<String>>>,
    reply: impl FnOnce(&mut TcpStream) + Send + 'static,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8(head).unwrap();
        let length: usize = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).unwrap();
        requests
            .lock()
            .unwrap()
            .push(head.lines().next().unwrap().to_owned());
        reply(&mut stream);
    })
}

/// Mirrors production's client shape: the pinned client carries a
/// client-level timeout, which in blocking reqwest is only a per-operation
/// inactivity bound (verified: a drip feed completes past it). The total
/// budget must come from the request path, not from here.
fn client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

fn body() -> serde_json::Value {
    serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "synthetic prompt"}],
        "stream": true,
    })
}

fn headers(stream: &mut TcpStream) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
}

/// The wall-clock gate itself, independent of network timing: a reader that
/// still has bytes must refuse them once the deadline has passed.
#[test]
fn the_deadline_gate_refuses_bytes_after_the_budget_is_spent() {
    let mut stream = DeadlineStream::new(
        std::io::repeat(b'x'),
        Instant::now() - Duration::from_millis(1),
    );
    let mut buf = [0u8; 8];
    let error = stream.read(&mut buf).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(error.to_string().contains("total deadline"));
}

/// Ten frames, one every 80 ms (~800 ms total): every individual read is far
/// inside any per-read inactivity bound, so only a total deadline can stop it.
#[test]
fn drip_fed_stream_over_the_total_budget_fails_instead_of_completing_slowly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = serve_once(listener, Arc::clone(&requests), |stream| {
        headers(stream);
        for index in 0..10 {
            let _ = write!(
                stream,
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"chunk{index}\"}}}}]}}\n\n"
            );
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(80));
        }
    });
    let started = Instant::now();
    let stream = send_chat_request(
        &client(),
        &format!("http://{address}/v1/chat/completions"),
        "synthetic-token",
        &body(),
        BUDGET,
    )
    .unwrap();
    let _failure = read_chat_stream(stream, &mut |_| {}).err().expect("over-budget stream must fail");
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(700),
        "a drip-fed stream past its total budget must fail promptly, took {elapsed:?}"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "over-budget stream must not be retried: {requests:?}");
    assert_eq!(requests[0], "POST /v1/chat/completions HTTP/1.1");
}

/// Headers arrive, then the server goes silent for a full second while holding
/// the socket open. The budget, not reqwest's 30 s blocking default, must end it.
#[test]
fn stream_that_goes_silent_after_headers_fails_within_the_budget() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = serve_once(listener, Arc::clone(&requests), |stream| {
        headers(stream);
        std::thread::sleep(Duration::from_millis(1000));
    });
    let started = Instant::now();
    let stream = send_chat_request(
        &client(),
        &format!("http://{address}/v1/chat/completions"),
        "synthetic-token",
        &body(),
        BUDGET,
    )
    .unwrap();
    assert!(read_chat_stream(stream, &mut |_| {}).is_err());
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert!(
        elapsed < Duration::from_millis(700),
        "a silent stream must fail within the budget, took {elapsed:?}"
    );
}

/// A complete stream inside the budget must still produce its artifact text.
#[test]
fn a_complete_stream_within_the_budget_returns_its_content() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = serve_once(listener, Arc::clone(&requests), |stream| {
        headers(stream);
        write!(
            stream,
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"hello\"}}}}]}}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
        )
        .unwrap();
        stream.flush().unwrap();
    });
    let started = Instant::now();
    let stream = send_chat_request(
        &client(),
        &format!("http://{address}/v1/chat/completions"),
        "synthetic-token",
        &body(),
        BUDGET,
    )
    .unwrap();
    let chat = read_chat_stream(stream, &mut |_| {}).unwrap();
    let elapsed = started.elapsed();
    server.join().unwrap();
    assert_eq!(chat.content, "hello");
    assert!(elapsed < Duration::from_millis(700), "complete stream took {elapsed:?}");
}

/// Content followed by a clean close without `[DONE]` is not a complete
/// artifact, however fast it arrived.
#[test]
fn closing_after_content_without_done_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let server = serve_once(listener, Arc::clone(&requests), |stream| {
        headers(stream);
        write!(
            stream,
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}}}}]}}\n\n"
        )
        .unwrap();
        stream.flush().unwrap();
    });
    let stream = send_chat_request(
        &client(),
        &format!("http://{address}/v1/chat/completions"),
        "synthetic-token",
        &body(),
        BUDGET,
    )
    .unwrap();
    let failure = read_chat_stream(stream, &mut |_| {}).err().expect("over-budget stream must fail");
    server.join().unwrap();
    assert!(
        format!("{failure:#}").contains("DONE"),
        "a stream closed before DONE must be rejected, got: {failure:#}"
    );
}
