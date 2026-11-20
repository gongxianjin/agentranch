//! Blocking OpenAI-compatible chat SSE decoder. No provider retry or invented text.
use anyhow::{Context, Result, bail, ensure};
use std::io::{BufReader, Read};
use sha2::{Digest, Sha256};
use super::live_output::{LiveOutput, MAX_STREAM_BYTES, MAX_TEXT_BYTES};

pub struct ChatStream {
    pub content: String,
    pub response_sha256: String,
}
struct Measured<R> { input: R, hash: Sha256, bytes: usize }
impl<R: Read> Read for Measured<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let count = self.input.read(out)?;
        self.bytes += count;
        if self.bytes > MAX_STREAM_BYTES {
            return Err(std::io::Error::other("model response exceeds 16MiB bound"));
        }
        self.hash.update(&out[..count]);
        Ok(count)
    }
}

/// The caller enters here only after accepting the model HTTP response.
pub fn read_chat_stream(input: impl Read, emit: &mut dyn FnMut(LiveOutput)) -> Result<ChatStream> {
    emit(LiveOutput::Connected);
    let measured = Measured { input, hash: Sha256::new(), bytes: 0 };
    let mut input = BufReader::new(measured);
    let mut content = String::new();
    let mut data = String::new();
    let mut line = Vec::new();
    let mut stopped = false;
    let mut after_cr = false;
    let mut first = true;
    let mut byte = [0];
    loop {
        ensure!(input.read(&mut byte).context("model SSE read failed")? != 0, "model SSE ended before DONE");
        let ch = byte[0];
        if after_cr && ch == b'\n' { after_cr = false; continue; }
        after_cr = ch == b'\r';
        if ch != b'\r' && ch != b'\n' { line.push(ch); continue; }
        let raw = std::mem::take(&mut line);
        let mut text = std::str::from_utf8(&raw).context("model SSE is not UTF-8")?;
        if first { text = text.trim_start_matches('\u{feff}'); first = false; }
        if !text.is_empty() {
            let (field, value) = text.split_once(':').unwrap_or((text, ""));
            if field == "data" {
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
                data.push('\n');
            }
            continue;
        }
        if data.is_empty() { continue; }
        data.pop();
        if data == "[DONE]" {
            ensure!(stopped && !content.is_empty(), "model SSE missing successful content/stop");
            emit(LiveOutput::ModelFinished);
            return Ok(ChatStream { content, response_sha256: hex::encode(input.into_inner().hash.finalize()) });
        }
        let event: serde_json::Value = serde_json::from_str(&data).context("invalid model SSE JSON")?;
        data.clear();
        ensure!(event.get("error").is_none(), "model SSE reported an error");
        let Some(choice) = event["choices"].as_array().and_then(|choices| choices.first()) else {
            // Usage-only trailing events carry no choices.
            ensure!(event["choices"].as_array().is_some_and(|c| c.is_empty()), "model SSE choices missing");
            continue;
        };
        if let Some(text) = choice["delta"]["content"].as_str() {
            ensure!(!stopped || text.is_empty(), "model SSE content after stop");
            ensure!(content.len() + text.len() <= MAX_TEXT_BYTES, "model chat content exceeds bound");
            if !text.is_empty() {
                content.push_str(text);
                emit(LiveOutput::Delta(text.to_owned()));
            }
        } else if let Some(text) = choice["delta"]["reasoning_content"].as_str().or_else(|| choice["delta"]["reasoning"]["text"].as_str()) {
            // Reasoning frames never join the artifact content; they only
            // keep the live panel honest while the model thinks (run
            // b242a25d: 58 s of silence read as a dead chain).
            if !text.is_empty() {
                emit(LiveOutput::Delta(text.to_owned()));
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            if reason != "stop" { bail!("model SSE did not finish normally"); }
            stopped = true;
        }
    }
}
