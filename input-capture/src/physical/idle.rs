//! Decides from session idle-time samples whether a person made input.

use std::time::{Duration, Instant};

/// Input that reached the compositor up to this long after an injection
/// was requested counts as that injection: the event still travels through
/// the emulation task and the compositor's input queue (milliseconds
/// normally, more on a loaded machine). Larger: physical input right after
/// an agent action goes unnoticed for longer; smaller: a slow injection is
/// mistaken for a person.
pub(super) const SLACK: Duration = Duration::from_millis(150);

/// The estimated time of the last input moves between samples by the
/// variation of the bus round trip; it must move by more than this to count
/// as new input.
pub(super) const TOLERANCE: Duration = Duration::from_millis(25);

/// The interval between samples.
pub(super) const POLL: Duration = Duration::from_millis(100);

// continuous movement moves the last input by about one POLL per sample,
// which must register as new input
const _: () = assert!(TOLERANCE.as_millis() < POLL.as_millis());

/// Input is reported one sample after it is seen, against the injections
/// known by then: an injection reaches the compositor before the caller
/// can call `injected`, and a sample taken in between must not report it.
#[derive(Default)]
pub(super) struct IdleDetector {
    /// estimated time of the latest input seen
    last_input: Option<Instant>,
    /// new input seen by the previous sample, not yet reported
    pending: Option<Instant>,
    last_injected: Option<Instant>,
}

impl IdleDetector {
    pub(super) fn injected(&mut self, at: Instant) {
        self.last_injected = self.last_injected.max(Some(at));
    }

    /// A sample: the session was `idle` at `now` (taken before the query,
    /// so the input time is never estimated later than it was). True: a
    /// person made input.
    pub(super) fn sample(&mut self, now: Instant, idle: Duration) -> bool {
        let physical = self.pending.take().is_some_and(|input| {
            self.last_injected
                .is_none_or(|injected| input > injected + SLACK)
        });
        // input before this process' clock began is old
        if let Some(input) = now.checked_sub(idle) {
            if self.last_input.is_some_and(|last| input > last + TOLERANCE) {
                self.pending = Some(input);
            }
            self.last_input = self.last_input.max(Some(input));
        }
        physical
    }
}

/// Whether two idle readings `elapsed` apart are milliseconds: without
/// input in between they grow by `elapsed` (seconds grow by 0 or 1).
pub(super) fn reports_ms(first: u32, second: u32, elapsed: Duration) -> bool {
    let grown = i64::from(second) - i64::from(first);
    let elapsed = elapsed.as_millis() as i64;
    elapsed >= 20 && (elapsed / 2..=elapsed * 2).contains(&grown)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// samples every POLL from `start` with the last input at `input`,
    /// returns whether any reported input
    fn run(d: &mut IdleDetector, start: Instant, samples: u32, input: Instant) -> bool {
        // every sample runs, also after one reported input
        (0..samples)
            .filter(|&i| {
                let now = start + POLL * i;
                d.sample(now, now.saturating_duration_since(input))
            })
            .count()
            > 0
    }

    #[test]
    fn no_event_without_input() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        assert!(!run(&mut d, t + 1000 * MS, 50, t));
    }

    #[test]
    fn event_on_input_without_injection() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        assert!(!run(&mut d, t + 1000 * MS, 5, t));
        let input = t + 1450 * MS;
        // seen at 1500, reported one sample later
        assert!(!d.sample(t + 1500 * MS, 50 * MS));
        assert!(d.sample(t + 1600 * MS, 150 * MS));
        assert!(!run(&mut d, t + 1700 * MS, 10, input));
    }

    #[test]
    fn event_on_input_after_slack() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        d.injected(t);
        assert!(!run(&mut d, t + 10 * MS, 3, t + 5 * MS));
        let input = t + SLACK + 10 * MS;
        assert!(run(&mut d, t + 300 * MS, 2, input));
    }

    #[test]
    fn no_event_right_after_injection() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        assert!(!run(&mut d, t, 3, t - 1000 * MS));
        let injected = t + 250 * MS;
        d.injected(injected);
        // the injection reaches the compositor a while later, within SLACK
        assert!(!run(&mut d, t + 300 * MS, 10, injected + SLACK - MS));
    }

    #[test]
    fn injection_seen_before_injected_is_called() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        d.injected(t);
        assert!(!run(&mut d, t + 100 * MS, 5, t + 2 * MS));
        // input of an injection at 1000 is sampled at 1001, the caller
        // reports it at 1003
        let at = t + 1000 * MS;
        assert!(!d.sample(t + 1001 * MS, Duration::ZERO));
        d.injected(at);
        assert!(!run(&mut d, t + 1101 * MS, 5, at));
    }

    #[test]
    fn later_injection_suppresses_input() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        assert!(!run(&mut d, t, 3, t - 1000 * MS));
        let input = t + 250 * MS;
        assert!(!d.sample(t + 300 * MS, 50 * MS));
        d.injected(t + 320 * MS);
        assert!(!run(&mut d, t + 400 * MS, 5, input));
    }

    #[test]
    fn continuous_input_keeps_reporting() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        assert!(!d.sample(t, 1000 * MS));
        // a person moving the mouse: input 10 ms before every sample
        let events = (1..10).filter(|&i| d.sample(t + POLL * i, 10 * MS)).count();
        assert_eq!(events, 8);
    }

    #[test]
    fn round_trip_jitter_is_not_input() {
        let t = Instant::now();
        let mut d = IdleDetector::default();
        let input = t - 1000 * MS;
        // the input estimate wobbles by up to TOLERANCE
        for i in 0..20 {
            let now = t + POLL * i;
            let wobble = if i % 2 == 0 {
                TOLERANCE
            } else {
                Duration::ZERO
            };
            assert!(!d.sample(now, now - input - wobble));
        }
    }

    #[test]
    fn idle_units() {
        assert!(reports_ms(5000, 5101, 100 * MS));
        assert!(!reports_ms(5, 5, 100 * MS));
        assert!(!reports_ms(5, 6, 100 * MS));
        // input in between
        assert!(!reports_ms(5000, 30, 100 * MS));
        assert!(!reports_ms(0, 0, Duration::ZERO));
    }
}
