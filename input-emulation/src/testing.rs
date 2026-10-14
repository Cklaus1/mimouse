//! A backend that records what reaches it, for tests of [`InputEmulation`].

use async_trait::async_trait;
use input_event::Event;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::normalize::Steps;
use crate::{Edge, Emulation, EmulationError, EmulationHandle, InputEmulation};

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

/// Records every call; `steps` and whether it can place the pointer are
/// chosen per test.
pub(crate) struct Recorder {
    calls: UnboundedSender<Call>,
    steps: Steps,
    places_pointer: bool,
}

impl Recorder {
    /// an [`InputEmulation`] on a recorder and the calls it records
    pub(crate) fn emulation(steps: Steps, places_pointer: bool) -> (InputEmulation, Calls) {
        let (calls, rx) = unbounded_channel();
        let recorder = Recorder {
            calls,
            steps,
            places_pointer,
        };
        (
            InputEmulation::with_emulation(Box::new(recorder)),
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
        Ok(())
    }

    fn steps(&self) -> Steps {
        self.steps
    }

    fn enter_at(
        &mut self,
        handle: EmulationHandle,
        _: Edge,
        _: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        self.record(Call::EnterAt(handle));
        Ok(self.places_pointer.then_some((0, 0)))
    }

    async fn create(&mut self, handle: EmulationHandle) {
        self.record(Call::Create(handle));
    }

    async fn destroy(&mut self, handle: EmulationHandle) {
        self.record(Call::Destroy(handle));
    }

    async fn terminate(&mut self) {
        self.record(Call::Terminate);
    }
}
