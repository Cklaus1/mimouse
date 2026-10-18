//! Backends for tests of [`InputEmulation`]: [`Recorder`] records what
//! reaches any backend, [`Fake`] stands in for a real one.

use async_trait::async_trait;
use input_event::Event;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::normalize::Steps;
use crate::text::Keystroke;
use crate::{
    Backend, Edge, Emulation, EmulationError, EmulationHandle, InputEmulation, Rect, Selected,
    TextSupport, Typed,
};

/// One call the layer made on the backend.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Call {
    Consume(EmulationHandle, Event),
    EnterAt(EmulationHandle),
    PointerTo(EmulationHandle, i32, i32),
    TypeText(EmulationHandle, String),
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

    fn text_support(&self) -> TextSupport {
        self.backend.text_support()
    }

    fn desktop(&self) -> Option<Vec<Rect>> {
        self.backend.desktop()
    }

    fn pointer_to(
        &mut self,
        handle: EmulationHandle,
        x: i32,
        y: i32,
    ) -> Result<bool, EmulationError> {
        self.record(Call::PointerTo(handle, x, y));
        self.backend.pointer_to(handle, x, y)
    }

    async fn type_text(
        &mut self,
        handle: EmulationHandle,
        text: &str,
    ) -> Result<Option<Typed>, EmulationError> {
        self.record(Call::TypeText(handle, text.to_owned()));
        self.backend.type_text(handle, text).await
    }

    fn keystroke(&self, c: char) -> Option<Keystroke> {
        self.backend.keystroke(c)
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

/// the display of a [`Fake`] that places the pointer
pub(crate) const FAKE_DISPLAY: Rect = Rect {
    x: -100,
    y: 0,
    width: 1000,
    height: 500,
};

pub(crate) const KEY_LEFTSHIFT: u32 = 42;
pub(crate) const KEY_RIGHTALT: u32 = 100;
pub(crate) const KEY_A: u32 = 30;
pub(crate) const KEY_E: u32 = 18;

/// A backend with the given `steps` that places the pointer (at (0, 0)
/// where a cursor enters, anywhere on [`FAKE_DISPLAY`]) if
/// `places_pointer`. Its layout types `a`, `A` (Shift) and `€` (Shift and
/// AltGr, as an unusual layout).
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

    fn text_support(&self) -> TextSupport {
        TextSupport::Layout
    }

    fn desktop(&self) -> Option<Vec<Rect>> {
        self.places_pointer.then(|| vec![FAKE_DISPLAY])
    }

    fn pointer_to(&mut self, _: EmulationHandle, _: i32, _: i32) -> Result<bool, EmulationError> {
        Ok(self.places_pointer)
    }

    fn keystroke(&self, c: char) -> Option<Keystroke> {
        let (modifiers, key, depressed) = match c {
            'a' => ([0, 0, 0], KEY_A, 0),
            'A' => ([KEY_LEFTSHIFT, 0, 0], KEY_A, 1),
            '€' => ([KEY_LEFTSHIFT, KEY_RIGHTALT, 0], KEY_E, 0x81),
            _ => return None,
        };
        Some(Keystroke {
            modifiers,
            key,
            depressed,
        })
    }

    async fn create(&mut self, _: EmulationHandle) {}

    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {}
}
