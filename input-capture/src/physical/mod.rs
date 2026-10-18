//! Input a person makes on this machine's own keyboard and mouse, as
//! opposed to input mimouse (or anything else) injects. The agent layer
//! stops as soon as a person touches the machine.
//!
//! Windows (low-level hooks) and macOS (listen-only event tap) tell injected
//! from physical events exactly ("hook"). Linux has no such API for an
//! unprivileged session client: it watches the session idle time and
//! reports input that happened clearly after the last injection ("idle").

use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::watch;

#[cfg(any(all(unix, not(target_os = "macos")), test))]
mod idle;
#[cfg(all(unix, not(target_os = "macos")))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

#[cfg(all(unix, not(target_os = "macos")))]
use linux as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
use windows as platform;

#[derive(Debug, Error)]
pub enum PhysicalError {
    /// this machine (or its permissions) offers no way to tell physical input
    #[error("physical input detection unsupported: {0}")]
    Unsupported(String),
    #[error("physical input detection failed: {0}")]
    Failed(String),
}

/// An injection that runs longer than this (typed text) calls
/// [`PhysicalInput::injected`] at least this often while it runs.
pub const INJECTED_EVERY: Duration = Duration::from_millis(50);

/// Watches for physical input while it lives.
pub struct PhysicalInput {
    events: watch::Receiver<()>,
    watcher: platform::Watcher,
}

impl PhysicalInput {
    /// Starts watching for input a person makes on this machine (not
    /// injected); stops on drop. Blocks until the watcher runs: a few
    /// milliseconds for the hooks; on Linux until the session bus answered
    /// the first idle-time queries, at most `PROBE_TIMEOUT` (2 s).
    pub fn watch() -> Result<PhysicalInput, PhysicalError> {
        let (tx, events) = watch::channel(());
        let watcher = platform::Watcher::start(tx)?;
        Ok(PhysicalInput { events, watcher })
    }

    /// The caller injected input at `at`; heuristic watchers ignore input
    /// until `at + grace`. Call it within `POLL` (100 ms) of the injection,
    /// with `at` no later than the moment the injection was requested.
    pub fn injected(&self, at: Instant) {
        self.watcher.injected(at);
    }

    /// Next physical input since the last call (several coalesce into
    /// one); never returns None while watching (cancel-safe). None: the
    /// watcher stopped (the OS disabled the tap, the idle monitor went away).
    pub async fn next(&mut self) -> Option<()> {
        self.events.changed().await.ok()
    }

    /// "hook" (exact) or "idle" (heuristic)
    pub fn method(&self) -> &'static str {
        platform::METHOD
    }
}
