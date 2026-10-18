//! The [`Desk`] of the running service: input emulation, the `desktop`
//! crate, the clipboard task and the physical-input watcher.

use std::time::{Duration, Instant};

use input_capture::PhysicalInput;
use input_event::Event;
use tokio::task::spawn_blocking;

use super::{
    proto::{ErrorCode, TextSupport},
    target::{Clip, Desk, Emulating, Refused, Watch},
};
use crate::{
    clipboard::{AgentClipboard, ClipboardData, MIME_FILES, MIME_HTML, MIME_PNG, MIME_TEXT},
    emulation::{AgentError, EmulationAgent},
};

pub(crate) struct ServiceDesk {
    emulation: EmulationAgent,
    screen: desktop::Screen,
    clipboard: AgentClipboard,
}

impl ServiceDesk {
    pub(crate) fn new(emulation: EmulationAgent, clipboard: AgentClipboard) -> Self {
        Self {
            emulation,
            screen: desktop::Screen::new(),
            clipboard,
        }
    }
}

impl From<AgentError> for Refused {
    fn from(e: AgentError) -> Self {
        match e {
            AgentError::EmulationOff => Refused::new(
                ErrorCode::EmulationOff,
                "input emulation is not running on this machine (`mimouse cli enable-emulation`)",
            ),
            AgentError::Unsupported => Refused::new(
                ErrorCode::Unsupported,
                "this machine's emulation backend cannot do that",
            ),
            AgentError::Failed(e) => Refused::new(ErrorCode::Internal, e),
        }
    }
}

/// the clipboard's name for `mime`; `text/plain` means UTF-8 text
fn canonical(mime: &str) -> &str {
    match mime {
        "text/plain" | "text/plain;charset=UTF-8" | "UTF8_STRING" => MIME_TEXT,
        other => other,
    }
}

impl Desk for ServiceDesk {
    type Watch = PhysicalInput;

    async fn input(&self, session: u64, event: Event) -> Result<(), Refused> {
        Ok(self.emulation.agent_event(session, event).await?)
    }

    async fn pointer_to(
        &self,
        session: u64,
        x: i32,
        y: i32,
        size: (u32, u32),
    ) -> Result<(), Refused> {
        Ok(self.emulation.agent_pointer_to(session, x, y, size).await?)
    }

    async fn type_text(&self, session: u64, text: &str) -> Result<String, Refused> {
        Ok(self.emulation.agent_type(session, text.to_owned()).await?)
    }

    async fn hostname(&self) -> String {
        crate::pair::local_name().await
    }

    fn end(&self, session: u64) {
        self.emulation.agent_end(session);
    }

    async fn emulation(&self) -> Option<Emulating> {
        let info = self.emulation.agent_info().await?;
        Some(Emulating {
            backend: info.backend,
            absolute: info.absolute,
            text: match info.text {
                input_emulation::TextSupport::Exact => TextSupport::Exact,
                input_emulation::TextSupport::Layout => TextSupport::Layout,
                input_emulation::TextSupport::None => TextSupport::None,
            },
        })
    }

    fn can_capture(&self) -> bool {
        self.screen.method().is_some()
    }

    async fn capture(&self) -> Result<desktop::Image, Refused> {
        Ok(self.screen.capture().await?)
    }

    async fn settled(
        &self,
        settle: Duration,
        timeout: Duration,
    ) -> Result<desktop::Image, Refused> {
        Ok(desktop::settle(&self.screen, settle, timeout).await?)
    }

    async fn geometry(&self) -> Result<desktop::Geometry, Refused> {
        Ok(self.screen.geometry().await?)
    }

    async fn locked(&self) -> Option<bool> {
        desktop::locked().await
    }

    async fn idle(&self) -> Option<Duration> {
        desktop::idle().await
    }

    fn has_clipboard(&self) -> bool {
        self.clipboard.available()
    }

    async fn clip_get(&self, mime: Option<String>) -> Result<Clip, Refused> {
        let unavailable = |e| Refused::new(ErrorCode::ClipboardUnavailable, e);
        let data = self.clipboard.get().await.map_err(unavailable)?;
        let empty = || Refused::new(ErrorCode::Empty, "the clipboard is empty");
        let data = data.ok_or_else(empty)?;
        let available: Vec<String> = data.mime_types().map(str::to_owned).collect();
        let mime = match mime {
            Some(m) => canonical(&m).to_owned(),
            None => available
                .iter()
                .find(|m| *m == MIME_TEXT)
                .or_else(|| available.iter().find(|m| *m != MIME_FILES))
                .cloned()
                .ok_or_else(|| Refused::new(ErrorCode::Empty, "the clipboard holds only files"))?,
        };
        let Some(bytes) = data.get(&mime) else {
            return Err(Refused::new(
                ErrorCode::Empty,
                format!(
                    "the clipboard holds no {mime}; it has {}",
                    available.join(", ")
                ),
            ));
        };
        Ok(Clip {
            data: bytes.to_vec(),
            mime,
            available,
        })
    }

    async fn clip_set(&self, mime: String, data: Vec<u8>) -> Result<(), Refused> {
        let mime = canonical(&mime);
        if ![MIME_TEXT, MIME_HTML, MIME_PNG].contains(&mime) {
            return Err(Refused::new(
                ErrorCode::BadRequest,
                format!("{mime} is not one of {MIME_TEXT}, {MIME_HTML}, {MIME_PNG}"),
            ));
        }
        if data.is_empty() {
            return Err(Refused::new(ErrorCode::BadRequest, "no data"));
        }
        if mime != MIME_PNG && std::str::from_utf8(&data).is_err() {
            return Err(Refused::new(
                ErrorCode::BadRequest,
                format!("{mime} must be UTF-8"),
            ));
        }
        let mut contents = ClipboardData::default();
        contents.push(mime, data);
        self.clipboard
            .set(contents)
            .await
            .map_err(|e| Refused::new(ErrorCode::ClipboardUnavailable, e))
    }

    async fn watch(&self) -> Option<PhysicalInput> {
        // probing the session bus blocks for up to a few seconds on Linux
        match spawn_blocking(PhysicalInput::watch).await {
            Ok(Ok(watch)) => {
                log::info!("agent: watching for a person's input ({})", watch.method());
                Some(watch)
            }
            Ok(Err(e)) => {
                log::warn!("agent: a person's input cannot be detected: {e}");
                None
            }
            Err(e) => {
                log::warn!("agent: input watcher: {e}");
                None
            }
        }
    }
}

impl Watch for PhysicalInput {
    fn injected(&self, at: Instant) {
        PhysicalInput::injected(self, at);
    }

    async fn next(&mut self) -> Option<()> {
        PhysicalInput::next(self).await
    }
}
