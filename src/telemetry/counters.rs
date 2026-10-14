//! Event counters, bumped where the events happen (the service, capture and
//! emulation tasks, backend threads): relaxed atomic increments that never
//! wait. The telemetry task drains them into the current day.

use std::{
    sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering::Relaxed},
    time::Duration,
};

use crate::{clipboard::ClipboardData, ipc::BackendInUse};

use super::schema::{Backend, Day, Selection, SizeBuckets};

pub(crate) struct Counter(AtomicU64);

impl Counter {
    const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub(crate) fn bump(&self) {
        self.0.fetch_add(1, Relaxed);
    }

    fn take(&self) -> u64 {
        self.0.swap(0, Relaxed)
    }
}

/// schema names of the capture backends (`config::CaptureBackend`)
pub(super) const CAPTURE_BACKENDS: [&str; 6] = [
    "input-capture-portal",
    "layer-shell",
    "x11",
    "windows",
    "macos",
    "dummy",
];
/// schema names of the emulation backends (`config::EmulationBackend`)
pub(super) const EMULATION_BACKENDS: [&str; 7] = [
    "wlroots", "libei", "xdp", "x11", "windows", "macos", "dummy",
];

/// the backend in use: 0 none, else its index + 1, `AUTO` set if picked
/// automatically
struct InUse(AtomicU8);

const AUTO: u8 = 0x80;

impl InUse {
    const fn new() -> Self {
        Self(AtomicU8::new(0))
    }

    /// `backend.name` is the backend's `Display` name (`X11`, `MacOS`,
    /// `xdg-desktop-portal`), mapped to its schema name in `names`
    fn set(&self, backend: Option<&BackendInUse>, names: &[&str]) {
        let value = backend
            .and_then(|b| {
                let index = schema_index(&b.name, names)?;
                Some((index as u8 + 1) | if b.auto { AUTO } else { 0 })
            })
            .unwrap_or(0);
        self.0.store(value, Relaxed);
    }

    fn get(&self, names: &[&'static str]) -> Option<(&'static str, Selection)> {
        let value = self.0.load(Relaxed);
        let name = names.get(usize::from(value & !AUTO).checked_sub(1)?)?;
        let selection = match value & AUTO {
            0 => Selection::Configured,
            _ => Selection::Auto,
        };
        Some((name, selection))
    }

    fn backend(&self, names: &[&'static str]) -> Backend {
        let in_use = self.get(names);
        Backend {
            backend: in_use.map(|(name, _)| name.to_owned()),
            selection: in_use.map(|(_, selection)| selection),
        }
    }
}

/// index of the backend whose `Display` name is `name` in `names`
pub(super) fn schema_index(name: &str, names: &[&str]) -> Option<usize> {
    let name = name.to_ascii_lowercase();
    let name = match name.as_str() {
        "xdg-desktop-portal" => "xdp",
        name => name,
    };
    names.iter().position(|n| *n == name)
}

/// everything telemetry counts between two drains
pub(crate) struct Counters {
    /// telemetry is on: a crash record written now may be uploaded
    pub(super) enabled: AtomicBool,
    pub(crate) reconnects: Counter,
    pub(crate) ping_timeouts: Counter,
    pub(crate) watchdog_releases: Counter,
    pub(crate) capture_disables: Counter,
    pub(crate) emulation_disables: Counter,
    pub(crate) clipboard_failures: Counter,
    pub(crate) switch_edge: Counter,
    pub(crate) switch_hotkey: Counter,
    pub(crate) switch_cli_focus: Counter,
    approval_waits: [Counter; 3],
    clipboard_transfers: [Counter; 5],
    file_transfers: [Counter; 5],
    peers: AtomicU64,
    capture: InUse,
    emulation: InUse,
}

pub(crate) static COUNTERS: Counters = Counters::new();

impl Counters {
    pub(super) const fn new() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            reconnects: Counter::new(),
            ping_timeouts: Counter::new(),
            watchdog_releases: Counter::new(),
            capture_disables: Counter::new(),
            emulation_disables: Counter::new(),
            clipboard_failures: Counter::new(),
            switch_edge: Counter::new(),
            switch_hotkey: Counter::new(),
            switch_cli_focus: Counter::new(),
            approval_waits: [Counter::new(), Counter::new(), Counter::new()],
            clipboard_transfers: [
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
            ],
            file_transfers: [
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
                Counter::new(),
            ],
            peers: AtomicU64::new(0),
            capture: InUse::new(),
            emulation: InUse::new(),
        }
    }

    /// a wait for a permission dialog ended after `waited`
    pub(crate) fn approval_waited(&self, waited: Duration) {
        let bucket = match waited.as_secs() {
            0..10 => 0,
            10..60 => 1,
            _ => 2,
        };
        self.approval_waits[bucket].bump();
    }

    /// a clipboard was sent or received: its formats count as a clipboard
    /// transfer, its files as a file transfer
    pub(crate) fn transfer(&self, data: &ClipboardData) {
        if data.size() > 0 {
            self.clipboard_transfers[size_bucket(data.size() as u64)].bump();
        }
        if !data.files().is_empty() {
            self.file_transfers[size_bucket(data.files_size())].bump();
        }
    }

    /// the number of configured clients
    pub(crate) fn set_peers(&self, peers: usize) {
        self.peers.store(peers as u64, Relaxed);
    }

    pub(crate) fn set_capture(&self, backend: Option<&BackendInUse>) {
        self.capture.set(backend, &CAPTURE_BACKENDS);
    }

    pub(crate) fn set_emulation(&self, backend: Option<&BackendInUse>) {
        self.emulation.set(backend, &EMULATION_BACKENDS);
    }

    /// schema names of the capture and emulation backends in use
    pub(super) fn backends(&self) -> (Option<&'static str>, Option<&'static str>) {
        (
            self.capture.get(&CAPTURE_BACKENDS).map(|(n, _)| n),
            self.emulation.get(&EMULATION_BACKENDS).map(|(n, _)| n),
        )
    }

    /// adds the counts since the last drain to `day` and resets them; the
    /// backends and peers are the current ones
    pub(super) fn drain_into(&self, day: &mut Day) {
        let r = &mut day.reliability;
        add(&mut r.reconnects, &self.reconnects);
        add(&mut r.ping_timeouts, &self.ping_timeouts);
        add(&mut r.watchdog_releases, &self.watchdog_releases);
        add(&mut r.capture_disables, &self.capture_disables);
        add(&mut r.emulation_disables, &self.emulation_disables);
        add(&mut r.clipboard_failures, &self.clipboard_failures);
        let [lt_10s, lt_60s, ge_60s] = &self.approval_waits;
        add(&mut r.approval_waits.lt_10s, lt_10s);
        add(&mut r.approval_waits.lt_60s, lt_60s);
        add(&mut r.approval_waits.ge_60s, ge_60s);
        let u = &mut day.usage;
        add(&mut u.switches.edge, &self.switch_edge);
        add(&mut u.switches.hotkey, &self.switch_hotkey);
        add(&mut u.switches.cli_focus, &self.switch_cli_focus);
        add_sizes(&mut u.clipboard_transfers, &self.clipboard_transfers);
        add_sizes(&mut u.file_transfers, &self.file_transfers);
        u.peers = self.peers.load(Relaxed).min(super::schema::MAX_PEERS);
        day.capture = self.capture.backend(&CAPTURE_BACKENDS);
        day.emulation = self.emulation.backend(&EMULATION_BACKENDS);
    }

    /// drops the counts since the last drain
    pub(super) fn discard(&self) {
        self.drain_into(&mut Day::default());
    }
}

fn add(total: &mut u64, counter: &Counter) {
    *total = total
        .saturating_add(counter.take())
        .min(super::schema::MAX_COUNT);
}

fn add_sizes(sizes: &mut SizeBuckets, counters: &[Counter; 5]) {
    let [lt_1k, lt_64k, lt_1m, lt_64m, ge_64m] = counters;
    add(&mut sizes.lt_1k, lt_1k);
    add(&mut sizes.lt_64k, lt_64k);
    add(&mut sizes.lt_1m, lt_1m);
    add(&mut sizes.lt_64m, lt_64m);
    add(&mut sizes.ge_64m, ge_64m);
}

/// `< 1 KiB`, `< 64 KiB`, `< 1 MiB`, `< 64 MiB`, `>= 64 MiB`
fn size_bucket(bytes: u64) -> usize {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    match bytes {
        0..KIB => 0,
        KIB..65_536 => 1,
        65_536..MIB => 2,
        MIB..67_108_864 => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CaptureBackend, EmulationBackend};
    use clap::ValueEnum;

    /// every backend the service can report maps to a schema name, so a
    /// renamed or new backend is not silently sent as `null`
    #[test]
    fn every_backend_has_a_schema_name() {
        for backend in CaptureBackend::value_variants() {
            let name = input_capture::Backend::from(*backend).to_string();
            assert!(
                schema_index(&name, &CAPTURE_BACKENDS).is_some(),
                "capture backend {name}"
            );
        }
        for backend in EmulationBackend::value_variants() {
            let name = input_emulation::Backend::from(*backend).to_string();
            assert!(
                schema_index(&name, &EMULATION_BACKENDS).is_some(),
                "emulation backend {name}"
            );
        }
    }

    #[test]
    fn sizes_fall_into_the_schema_buckets() {
        let cases = [
            (0, 0),
            (1023, 0),
            (1024, 1),
            (65_535, 1),
            (65_536, 2),
            (1_048_575, 2),
            (1_048_576, 3),
            (67_108_863, 3),
            (67_108_864, 4),
            (u64::MAX, 4),
        ];
        for (bytes, bucket) in cases {
            assert_eq!(size_bucket(bytes), bucket, "{bytes} bytes");
        }
    }

    #[test]
    fn approval_waits_fall_into_the_schema_buckets() {
        let counters = Counters::new();
        for secs in [0, 9, 10, 59, 60, 3600] {
            counters.approval_waited(Duration::from_secs(secs));
        }
        counters.approval_waited(Duration::from_millis(9_999));
        let mut day = Day::default();
        counters.drain_into(&mut day);
        let waits = &day.reliability.approval_waits;
        assert_eq!((waits.lt_10s, waits.lt_60s, waits.ge_60s), (3, 2, 2));
    }

    /// a drain hands each event to exactly one day
    #[test]
    fn drained_counts_are_not_counted_again() {
        let counters = Counters::new();
        counters.reconnects.bump();
        counters.reconnects.bump();
        let mut first = Day::default();
        counters.drain_into(&mut first);
        counters.reconnects.bump();
        counters.drain_into(&mut first);
        let mut second = Day::default();
        counters.drain_into(&mut second);
        assert_eq!(first.reliability.reconnects, 3);
        assert_eq!(second.reliability.reconnects, 0);
    }

    #[test]
    fn backend_in_use_is_reported_with_its_selection() {
        let counters = Counters::new();
        counters.set_capture(Some(&BackendInUse {
            name: "X11".into(),
            auto: true,
        }));
        counters.set_emulation(Some(&BackendInUse {
            name: "xdg-desktop-portal".into(),
            auto: false,
        }));
        assert_eq!(counters.backends(), (Some("x11"), Some("xdp")));
        let mut day = Day::default();
        counters.drain_into(&mut day);
        assert_eq!(day.capture.selection, Some(Selection::Auto));
        assert_eq!(day.emulation.selection, Some(Selection::Configured));
        counters.set_capture(None);
        assert_eq!(counters.backends().0, None);
    }
}
