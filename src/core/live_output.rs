//! Ephemeral model text, independent of Store admission and final stdout.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use super::owner_producer::StreamReadOutcome;

pub const MAX_TEXT_BYTES: usize = 1_048_576;
/// Raw SSE byte ceiling. Run b242a25d (2026-11-20): a reasoning model's
/// JSON-wrapped frames exceeded the old 1MiB raw bound at 58 s with zero
/// content extracted, killing an otherwise healthy stream. Content keeps
/// its own MAX_TEXT_BYTES cap; this only bounds transport memory.
pub const MAX_STREAM_BYTES: usize = 16 * MAX_TEXT_BYTES;
const PREFIX: &[u8] = b"AO-LIVE ";
const MAX_LINE: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case", deny_unknown_fields)]
pub enum LiveOutput {
    Connected,
    Delta(String),
    ModelFinished,
    Error(String),
}

/// A complete line is the only boundary interpreted as model output.
#[derive(Default)]
pub struct LiveLines { pending: Vec<u8>, text_bytes: usize, ordinary: bool }
impl LiveLines {
    pub fn push(&mut self, bytes: &[u8], emit: &mut dyn FnMut(LiveOutput), diagnostic: &mut dyn FnMut(&[u8])) -> Result<()> {
        for (index, &byte) in bytes.iter().enumerate() {
            if self.ordinary {
                diagnostic(&[byte]);
                if byte == b'\n' {self.ordinary = false;}
                continue;
            }
            self.pending.push(byte);
            if !PREFIX.starts_with(&self.pending) && !self.pending.starts_with(PREFIX) {
                diagnostic(&std::mem::take(&mut self.pending));
                self.ordinary = byte != b'\n';
                continue;
            }
            if self.pending.len() > MAX_LINE {
                diagnostic(&std::mem::take(&mut self.pending));
                diagnostic(&bytes[index + 1..]);
                bail!("live output line exceeds 64KiB");
            }
            if byte == b'\n' {
                let line = std::mem::take(&mut self.pending);
                if let Some(json) = line.strip_prefix(PREFIX) {
                    let event: LiveOutput = match serde_json::from_slice(json).context("invalid live output line") {
                        Ok(event) => event,
                        Err(error) => {
                            diagnostic(&line);
                            diagnostic(&bytes[index + 1..]);
                            return Err(error);
                        }
                    };
                    if let LiveOutput::Delta(text) = &event {
                        if self.text_bytes.saturating_add(text.len()) > MAX_TEXT_BYTES {
                            diagnostic(&line);
                            diagnostic(&bytes[index + 1..]);
                            bail!("live output exceeds 1MiB");
                        }
                        self.text_bytes += text.len();
                    }
                    emit(event);
                } else { diagnostic(&line); }
            }
        }
        Ok(())
    }
    pub fn finish(&mut self, diagnostic: &mut dyn FnMut(&[u8])) -> Result<()> {
        if self.pending.starts_with(PREFIX) {
            diagnostic(&std::mem::take(&mut self.pending));
            bail!("incomplete live output line");
        }
        diagnostic(&std::mem::take(&mut self.pending));
        Ok(())
    }
}

/// Keep each line small even when a provider sends a large single delta.
pub fn write_live(output: &mut impl Write, event: &LiveOutput) -> Result<()> {
    if let LiveOutput::Delta(text) = event {
        let mut rest = text.as_str();
        while !rest.is_empty() {
            let mut end = rest.len().min(4096);
            while !rest.is_char_boundary(end) { end -= 1; }
            write_line(output, &LiveOutput::Delta(rest[..end].into()))?;
            rest = &rest[end..];
        }
        return Ok(());
    }
    write_line(output, event)
}
fn write_line(output: &mut impl Write, event: &LiveOutput) -> Result<()> {
    output.write_all(PREFIX)?;
    serde_json::to_writer(&mut *output, event)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

/// Tee, not filter: final evidence retains the exact original stderr prefix.
/// Shared by the Linux adapter and platform-independent pipe tests.
pub fn capture_with_live(mut input: impl Read, forward: &mut dyn FnMut(&[u8])) -> StreamReadOutcome {
    let mut result = StreamReadOutcome { prefix: vec![], total_read_bytes: 0, eof: false, error: None };
    let mut lines = LiveLines::default();
    let mut live_ok = true;
    let mut buffer = [0; 8192];
    loop {
        match input.read(&mut buffer) {
            Ok(0) => { result.eof = true; break; }
            Ok(count) => {
                result.total_read_bytes += count as u64;
                let keep = (4096 - result.prefix.len()).min(count);
                result.prefix.extend_from_slice(&buffer[..keep]);
                if live_ok {
                    let mut emit = |event| { let mut wire = vec![]; if write_live(&mut wire, &event).is_ok() { forward(&wire); } };
                    live_ok = lines.push(&buffer[..count], &mut emit, &mut |_| {}).is_ok();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => { result.error = Some(e.to_string()); break; }
        }
    }
    if live_ok { live_ok = lines.finish(&mut |_| {}).is_ok(); }
    if !live_ok {
        let mut wire = vec![];
        let _ = write_live(&mut wire, &LiveOutput::Error("Worker 输出通道中断".into()));
        forward(&wire);
    }
    result
}
