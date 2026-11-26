//! Nonblocking workbench interface. A successful submit means only acceptance.

use crate::core::service::{AppSnapshot, CommandResult, LiveOutputEvent, ServiceHandle, SubmitError, UiCommand};
use std::sync::Arc;

/// `Send` lets the single bridge move its owned backend to the owner loop
/// thread; the trait is intentionally not `Sync` because one consumer owns it.
pub trait Backend: Send {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError>;
    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>);
    fn snapshot(&mut self) -> Arc<AppSnapshot>;
    /// Non-blocking stop request. The core keeps draining until it publishes
    /// its stopped snapshot; `disconnect` remains the only blocking cleanup.
    fn request_stop(&self) {}
    fn disconnect(&self);
}

impl Backend for ServiceHandle {
    fn try_submit(&self, command: UiCommand) -> Result<u64, SubmitError> {
        ServiceHandle::try_submit(self, command)
    }

    fn drain_updates(&mut self) -> (Vec<LiveOutputEvent>, Vec<CommandResult>) {
        ServiceHandle::drain_updates(self)
    }

    fn snapshot(&mut self) -> Arc<AppSnapshot> {
        ServiceHandle::snapshot(self)
    }

    fn request_stop(&self) {
        ServiceHandle::stop(self);
    }

    fn disconnect(&self) {
        ServiceHandle::disconnect(self);
    }
}
