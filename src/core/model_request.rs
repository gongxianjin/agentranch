//! Shared model HTTP request path.
//!
//! The call site hands in the blocking client (production keeps its pinned-TLS
//! client) so this module owns only the transport shape and the total budget.
//!
//! Why the budget needs more than a timeout setting: in the pinned reqwest
//! (0.13.5) a **client-level** timeout never bounds a streaming body — its
//! blocking reads recompute `Instant::now() + duration` per call, so a server
//! that keeps dribbling bytes stays inside every individual bound and the
//! whole stream can run past the budget (measured: 812 ms against a 300 ms
//! client-level timeout). The request-level timeout is applied by reqwest as
//! one `Sleep` over the body, and the reader below adds a version-independent
//! wall-clock gate with its own error text.
use anyhow::{Context, Result};
use std::io::{self, Read};
use std::time::{Duration, Instant};

/// Reader bound to an absolute instant: once the budget is spent, reads fail
/// closed instead of exposing more body bytes.
pub struct DeadlineStream<R> {
    inner: R,
    deadline: Instant,
}

impl<R> DeadlineStream<R> {
    pub fn new(inner: R, deadline: Instant) -> Self {
        Self { inner, deadline }
    }
}

impl DeadlineStream<reqwest::blocking::Response> {
    pub fn status(&self) -> u16 {
        self.inner.status().as_u16()
    }
}

impl<R: Read> Read for DeadlineStream<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "model response exceeded its total deadline",
            ));
        }
        self.inner.read(out)
    }
}

/// Sends one chat-completions POST and returns the response as a
/// deadline-bound reader for the caller's SSE decoder. The budget covers
/// connect, headers and body.
pub fn send_chat_request(
    client: &reqwest::blocking::Client,
    url: &str,
    token: &str,
    body: &serde_json::Value,
    timeout: Duration,
) -> Result<DeadlineStream<reqwest::blocking::Response>> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("model request budget overflows the system clock")?;
    let response = client
        .post(url)
        .bearer_auth(token)
        // Request-level, not client-level: only this form is applied as one
        // deadline over the streaming body (see module docs).
        .timeout(timeout)
        .json(body)
        .send()
        .context("model HTTP request failed")?;
    Ok(DeadlineStream::new(response, deadline))
}
