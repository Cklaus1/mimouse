//! Backends for tests of [`InputEmulation`]: [`Recorder`] records what
//! reaches any backend, [`Fake`] stands in for a real one.

use async_trait::async_trait;
use input_event::Event;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::normalize::Steps;
use crate::{Backend, Edge, Emulation, EmulationError, EmulationHandle, InputEmulation, Selected};

/// One call the layer made on the backend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Call {
    Consume(EmulationHandle, Event),
    EnterAt(EmulationHandle),
    Create(EmulationHandle),
    Destroy(EmulationHandle),
    Terminate,
}

/// The calls a [`Recorder`] received.
pub(crate) struct Calls(UnboundedReceiver<Call>);

impl Calls {
    /// the calls since the last `take`
    pub(crate) fn take(&mut self) -> Vec<Call> {
        std::iter::from_fn(|| self.0.try_recv().ok()).collect()
    }
}

/// Records every call, then hands it to the backend it wraps.
pub(crate) struct Recorder {
    calls: UnboundedSender<Call>,
    backend: Box<dyn Emulation>,
}

impl Recorder {
    /// an [`InputEmulation`] on a recorded [`Fake`] and the calls it records
    pub(crate) fn emulation(steps: Steps, places_pointer: bool) -> (InputEmulation, Calls) {
        Self::wrap(Box::new(Fake {
            steps,
            places_pointer,
            stall_at: None,
        }))
    }

    /// an [`InputEmulation`] on `backend` and the calls that reach it
    pub(crate) fn wrap(backend: Box<dyn Emulation>) -> (InputEmulation, Calls) {
        let (calls, rx) = unbounded_channel();
        let recorder = Recorder { calls, backend };
        let configured = Selected {
            backend: Backend::Dummy,
            auto: false,
        };
        (
            InputEmulation::with_emulation(Box::new(recorder), configured),
            Calls(rx),
        )
    }

    fn record(&self, call: Call) {
        // the test may have dropped `Calls` already
        let _ = self.calls.send(call);
    }
}

#[async_trait]
impl Emulation for Recorder {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        self.record(Call::Consume(handle, event));
        self.backend.consume(event, handle).await
    }

    fn steps(&self) -> Steps {
        self.backend.steps()
    }

    fn enter_at(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        self.record(Call::EnterAt(handle));
        self.backend.enter_at(handle, edge, fraction)
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.record(Call::Create(handle));
        self.backend.create(handle).await
    }

    async fn destroy(&mut self, handle: EmulationHandle) {
        self.record(Call::Destroy(handle));
        self.backend.destroy(handle).await
    }

    async fn terminate(&mut self) {
        self.record(Call::Terminate);
        self.backend.terminate().await
    }
}

/// A backend with the given `steps` that places the pointer at (0, 0) if
/// `places_pointer`.
pub(crate) struct Fake {
    pub(crate) steps: Steps,
    pub(crate) places_pointer: bool,
    /// `consume` call (counting from 0) that never completes, as if the
    /// caller dropped it mid-way; later calls complete
    pub(crate) stall_at: Option<usize>,
}

#[async_trait]
impl Emulation for Fake {
    async fn consume(&mut self, _: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match &mut self.stall_at {
            Some(0) => {
                self.stall_at = None;
                std::future::pending().await
            }
            Some(n) => *n -= 1,
            None => {}
        }
        Ok(())
    }

    fn steps(&self) -> Steps {
        self.steps
    }

    fn enter_at(
        &mut self,
        _: EmulationHandle,
        _: Edge,
        _: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        Ok(self.places_pointer.then_some((0, 0)))
    }

    async fn create(&mut self, _: EmulationHandle) {}

    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {}
}
