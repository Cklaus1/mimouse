use std::sync::{Arc, Mutex, MutexGuard};

use async_trait::async_trait;
use input_event::Event;

use crate::error::EmulationError;

use super::{Edge, Emulation, EmulationHandle, TextSupport, Typed};

/// What a recording dummy backend ([`crate::InputEmulation::dummy`]) was
/// asked to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Received {
    Create(EmulationHandle),
    Destroy(EmulationHandle),
    Event(EmulationHandle, Event),
    /// text typed for a handle, newlines normalized
    Typed(EmulationHandle, String),
    Terminate,
}

/// The shared log of a recording dummy backend, for tests of the layers
/// above emulation. Clones share the log.
#[derive(Clone, Debug, Default)]
pub struct DummyRecord(Arc<Mutex<Log>>);

#[derive(Debug, Default)]
struct Log {
    received: Vec<Received>,
    fail_next: bool,
}

impl DummyRecord {
    /// everything received since the last call
    pub fn take(&self) -> Vec<Received> {
        std::mem::take(&mut self.log().received)
    }

    /// The next event or text fails with [`EmulationError::EndOfStream`],
    /// as when a backend's connection is lost.
    pub fn fail_next(&self) {
        self.log().fail_next = true;
    }

    fn log(&self) -> MutexGuard<'_, Log> {
        // a panic while recording leaves a usable log
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn push(&self, received: Received) {
        self.log().received.push(received);
    }

    fn take_failure(&self) -> bool {
        std::mem::take(&mut self.log().fail_next)
    }
}

/// Discards everything; with a [`DummyRecord`], records it instead.
#[derive(Default)]
pub(crate) struct DummyEmulation {
    record: Option<DummyRecord>,
}

impl DummyEmulation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn recording(record: DummyRecord) -> Self {
        Self {
            record: Some(record),
        }
    }

    fn record(&self, received: Received) {
        if let Some(record) = &self.record {
            record.push(received);
        }
    }

    /// an event or text, failing once after [`DummyRecord::fail_next`]
    fn record_input(&self, received: Received) -> Result<(), EmulationError> {
        if self.record.as_ref().is_some_and(DummyRecord::take_failure) {
            return Err(EmulationError::EndOfStream);
        }
        self.record(received);
        Ok(())
    }
}

#[async_trait]
impl Emulation for DummyEmulation {
    async fn consume(
        &mut self,
        event: Event,
        client_handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            // never log what a peer types
            Event::Keyboard(_) => log::debug!("received keyboard event: ({client_handle})"),
            Event::Pointer(_) => log::debug!("received event: ({client_handle}) {event}"),
        }
        self.record_input(Received::Event(client_handle, event))
    }
    fn enter_at(
        &mut self,
        client_handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        // there is no pointer to place
        log::debug!("received enter: ({client_handle}) {edge} edge at {fraction}");
        Ok(None)
    }
    fn text_support(&self) -> TextSupport {
        TextSupport::Exact
    }
    async fn type_text(
        &mut self,
        client_handle: EmulationHandle,
        text: &str,
    ) -> Result<Option<Typed>, EmulationError> {
        // never log what an agent types
        log::debug!("received text: ({client_handle})");
        self.record_input(Received::Typed(
            client_handle,
            crate::text::chars(text).collect(),
        ))?;
        Ok(Some(Typed::default()))
    }
    async fn create(&mut self, handle: EmulationHandle) {
        self.record(Received::Create(handle));
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.record(Received::Destroy(handle));
    }
    async fn terminate(&mut self) {
        self.record(Received::Terminate);
    }
}
