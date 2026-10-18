//! A [`Desk`] for tests: records input, can hold it, and signals a
//! person's input on demand.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::{Duration, Instant},
};

use input_event::Event;
use tokio::sync::{Notify, mpsc};

use super::{
    proto::{ErrorCode, Grant, Grants, TextSupport},
    target::{Clip, Desk, Emulating, Refused, Watch},
};

#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) input: RefCell<Vec<(u64, Event)>>,
    pub(crate) ended: RefCell<Vec<u64>>,
    /// signals a person's input to the running session's watch
    pub(crate) person: RefCell<Option<mpsc::UnboundedSender<()>>>,
    /// while set, `input` waits for it before returning
    pub(crate) gate: RefCell<Option<Rc<Notify>>>,
    pub(crate) inputs_started: Cell<usize>,
    /// while set, `locked` waits for it before answering
    pub(crate) lock_gate: RefCell<Option<Rc<Notify>>>,
    pub(crate) lock_queries: Cell<usize>,
    /// how long `type_text` takes
    pub(crate) typing: Cell<Duration>,
    /// injections the watch heard of
    pub(crate) injections: Cell<usize>,
}

#[derive(Clone, Default)]
pub(crate) struct FakeDesk(pub(crate) Rc<Shared>);

pub(crate) struct FakeWatch(mpsc::UnboundedReceiver<()>, Rc<Shared>);

impl Watch for FakeWatch {
    fn injected(&self, _: Instant) {
        self.1.injections.set(self.1.injections.get() + 1);
    }
    async fn next(&mut self) -> Option<()> {
        self.0.recv().await
    }
}

impl Desk for FakeDesk {
    type Watch = FakeWatch;
    async fn input(&self, session: u64, event: Event) -> Result<(), Refused> {
        self.0.inputs_started.set(self.0.inputs_started.get() + 1);
        let gate = self.0.gate.borrow().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.0.input.borrow_mut().push((session, event));
        Ok(())
    }
    async fn pointer_to(&self, _: u64, _: i32, _: i32, _: (u32, u32)) -> Result<(), Refused> {
        Ok(())
    }
    async fn type_text(&self, _: u64, _: &str) -> Result<String, Refused> {
        tokio::time::sleep(self.0.typing.get()).await;
        Ok(String::new())
    }
    async fn hostname(&self) -> String {
        "fake".into()
    }
    fn end(&self, session: u64) {
        self.0.ended.borrow_mut().push(session);
    }
    async fn emulation(&self) -> Option<Emulating> {
        Some(Emulating {
            backend: "fake".into(),
            absolute: true,
            text: TextSupport::Exact,
        })
    }
    fn can_capture(&self) -> bool {
        false
    }
    async fn capture(&self) -> Result<desktop::Image, Refused> {
        Err(Refused::new(ErrorCode::Unsupported, "no screen"))
    }
    async fn settled(&self, _: Duration, _: Duration) -> Result<desktop::Image, Refused> {
        self.capture().await
    }
    async fn geometry(&self) -> Result<desktop::Geometry, Refused> {
        Ok(desktop::Geometry {
            width: 100,
            height: 100,
            monitors: vec![],
        })
    }
    async fn locked(&self) -> Option<bool> {
        self.0.lock_queries.set(self.0.lock_queries.get() + 1);
        let gate = self.0.lock_gate.borrow().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        Some(false)
    }
    async fn idle(&self) -> Option<Duration> {
        None
    }
    fn has_clipboard(&self) -> bool {
        false
    }
    async fn clip_get(&self, _: Option<String>) -> Result<Clip, Refused> {
        Err(Refused::new(ErrorCode::ClipboardUnavailable, "none"))
    }
    async fn clip_set(&self, _: String, _: Vec<u8>) -> Result<(), Refused> {
        Err(Refused::new(ErrorCode::ClipboardUnavailable, "none"))
    }
    async fn watch(&self) -> Option<FakeWatch> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.0.person.borrow_mut() = Some(tx);
        Some(FakeWatch(rx, self.0.clone()))
    }
}

pub(crate) fn allow_all() -> Grants {
    Grants {
        input: Grant::Allow,
        screen: Grant::Allow,
        clipboard: Grant::Allow,
    }
}
