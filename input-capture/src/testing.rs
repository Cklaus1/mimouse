//! Backends for tests of [`InputCapture`]: [`Recorder`] records what passes
//! between the layer and any backend, [`Fake`] stands in for an OS backend.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::event_queue::{self, EventReceiver, EventSender};
use crate::{Capture, CaptureError, CaptureEvent, Chord, EdgeRange, InputCapture, Position};

/// A call the layer made on the backend, or an event the backend reported.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Call {
    Create(Position, Option<EdgeRange>),
    Destroy(Position),
    Release,
    Focus(Position, Option<EdgeRange>),
    SetHotkeys(Vec<(Chord, Position)>),
    Terminate,
    Report(Position, CaptureEvent),
}

/// The calls a [`Recorder`] received.
pub(crate) struct Calls(UnboundedReceiver<Call>);

impl Calls {
    /// the calls since the last `take`
    pub(crate) fn take(&mut self) -> Vec<Call> {
        std::iter::from_fn(|| self.0.try_recv().ok()).collect()
    }
}

/// Records every call and report, passing them between the layer and the
/// backend it wraps.
pub(crate) struct Recorder {
    calls: UnboundedSender<Call>,
    backend: Box<dyn Capture + Send>,
}

impl Recorder {
    /// an [`InputCapture`] on `backend` and what passes between them
    pub(crate) fn wrap(backend: Box<dyn Capture + Send>) -> (InputCapture, Calls) {
        let (calls, rx) = unbounded_channel();
        let recorder = Recorder { calls, backend };
        (InputCapture::with_backend(Box::new(recorder)), Calls(rx))
    }

    fn record(&self, call: Call) {
        // the test may have dropped `Calls` already
        let _ = self.calls.send(call);
    }
}

#[async_trait]
impl Capture for Recorder {
    async fn create(
        &mut self,
        pos: Position,
        range: Option<EdgeRange>,
    ) -> Result<(), CaptureError> {
        self.record(Call::Create(pos, range));
        self.backend.create(pos, range).await
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.record(Call::Destroy(pos));
        self.backend.destroy(pos).await
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.record(Call::Release);
        self.backend.release().await
    }

    async fn focus(&mut self, pos: Position, range: Option<EdgeRange>) -> Result<(), CaptureError> {
        self.record(Call::Focus(pos, range));
        self.backend.focus(pos, range).await
    }

    async fn set_hotkeys(&mut self, hotkeys: Vec<(Chord, Position)>) -> Result<(), CaptureError> {
        self.record(Call::SetHotkeys(hotkeys.clone()));
        self.backend.set_hotkeys(hotkeys).await
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.record(Call::Terminate);
        self.backend.terminate().await
    }
}

impl Stream for Recorder {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.backend.poll_next_unpin(cx);
        if let Poll::Ready(Some(Ok((pos, event)))) = item {
            self.record(Call::Report(pos, event));
        }
        item
    }
}

/// How a [`Fake`] answers `focus`.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Focus {
    /// starts capture and reports `Begin` (Windows, macOS)
    Starts,
    /// [`CaptureError::FocusByPointerPush`] (libei, layer-shell, X11)
    ByPointerPush,
    /// another error
    Fails,
}

/// A backend whose events come from a [`Script`] through the queue the
/// Windows and macOS OS callbacks use, with their limit.
pub(crate) struct Fake {
    queue: EventReceiver,
    /// `Begin`s of started focuses, reported before the queue
    focused: VecDeque<(Position, CaptureEvent)>,
    focus: Focus,
    errors: Arc<Mutex<VecDeque<CaptureError>>>,
}

/// What the OS callback of a [`Fake`] reports; the stream ends once it is
/// dropped.
pub(crate) struct Script {
    queue: EventSender,
    errors: Arc<Mutex<VecDeque<CaptureError>>>,
}

impl Script {
    /// queues `event` at `pos`; false if the full queue dropped it
    pub(crate) fn report(&self, pos: Position, event: CaptureEvent) -> bool {
        self.queue.send(pos, event)
    }

    /// the stream yields `error` once every queued event is out
    pub(crate) fn fail(&self, error: CaptureError) {
        self.errors.lock().unwrap().push_back(error);
    }
}

impl Fake {
    /// a backend answering `focus` as `focus`, whose queue drops what may
    /// be lost while `limit` events wait
    pub(crate) fn new(focus: Focus, limit: usize) -> (Self, Script) {
        let (tx, rx) = event_queue::channel(limit);
        let errors = Arc::<Mutex<VecDeque<_>>>::default();
        let fake = Fake {
            queue: rx,
            focused: VecDeque::new(),
            focus,
            errors: errors.clone(),
        };
        (fake, Script { queue: tx, errors })
    }
}

#[async_trait]
impl Capture for Fake {
    async fn create(&mut self, _: Position, _: Option<EdgeRange>) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn destroy(&mut self, _: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn focus(&mut self, pos: Position, _: Option<EdgeRange>) -> Result<(), CaptureError> {
        match self.focus {
            Focus::Starts => {
                // as if the cursor crossed in the middle of the range
                let begin = CaptureEvent::Begin(Some(u16::MAX / 2));
                self.focused.push_back((pos, begin));
                Ok(())
            }
            Focus::ByPointerPush => Err(CaptureError::FocusByPointerPush),
            Focus::Fails => Err(CaptureError::Io(std::io::ErrorKind::Other.into())),
        }
    }

    async fn set_hotkeys(&mut self, _: Vec<(Chord, Position)>) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for Fake {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.focused.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }
        match self.queue.poll_recv(cx) {
            Poll::Ready(event) => Poll::Ready(event.map(Ok)),
            Poll::Pending => match self.errors.lock().unwrap().pop_front() {
                Some(error) => Poll::Ready(Some(Err(error))),
                None => Poll::Pending,
            },
        }
    }
}
