use crate::error::EmulationError;

use super::{Emulation, TextSupport, error::WlrootsEmulationCreationError};
use async_trait::async_trait;
use bitflags::bitflags;
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{Interest, unix::AsyncFd};
use wayland_client::WEnum;
use wayland_client::backend::WaylandError;

use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_keyboard::{self, WlKeyboard};
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1 as XdgOutputManager,
    zxdg_output_v1::{self, ZxdgOutputV1 as XdgOutput},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 as VpManager,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1 as Vp,
};

use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 as VkManager,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1 as Vk,
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use input_event::{Event, KeyboardEvent, PointerEvent, scancode};

use super::error::WaylandBindError;
use super::geometry::{Rect, bounds, entry_point};
use super::text::{Keystroke, Layout};
use super::{Edge, EmulationHandle};

/// How long the compositor may take to send the keymap, and then the output
/// layout. A seat without a keyboard never sends a keymap; creation fails
/// then and is retried later.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a key-up or button-up waits for the compositor to read a full
/// connection. Discarding it would leave the key or button held; a
/// compositor that reads nothing for this long ends the backend, which is
/// then recreated.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(1);

/// the seat's keymap, uploaded to every virtual keyboard
struct Keymap {
    format: u32,
    fd: OwnedFd,
    size: u32,
    /// the contents: the compositor re-sends an unchanged keymap
    text: Vec<u8>,
}

impl Keymap {
    fn read(format: u32, fd: OwnedFd, size: u32) -> io::Result<Self> {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::from(fd);
        let mut text = vec![0; size as usize];
        file.read_exact_at(&mut text, 0)?;
        Ok(Self {
            format,
            fd: file.into(),
            size,
            text,
        })
    }
}

struct State {
    keymap: Option<Keymap>,
    /// the keys of `keymap`, for typing text
    layout: Layout,
    /// the seat's keyboard, which receives the keymap; one at most, since
    /// each gets every keymap event
    keyboard: Option<WlKeyboard>,
    input_for_client: HashMap<EmulationHandle, VirtualInput>,
    seat: wl_seat::WlSeat,
    qh: QueueHandle<Self>,
    vpm: VpManager,
    vkm: VkManager,
    /// for the output layout, needed to place the pointer
    xdg_output_manager: Option<XdgOutputManager>,
    /// outputs by global name
    outputs: HashMap<u32, Output>,
    /// the compositor answered the last `wl_display.sync`
    synced: bool,
}

/// an output and its logical geometry in the layout
struct Output {
    wl_output: WlOutput,
    xdg_output: XdgOutput,
    position: (i32, i32),
    size: (i32, i32),
}

// App State, implements Dispatch event handlers
pub(crate) struct WlrootsEmulation {
    conn: Connection,
    last_flush_failed: bool,
    /// the compositor read nothing for `RELEASE_TIMEOUT` while a release
    /// waited: the backend ends
    stalled: bool,
    state: State,
    queue: EventQueue<State>,
}

impl WlrootsEmulation {
    pub(crate) async fn new() -> Result<Self, WlrootsEmulationCreationError> {
        Self::start(Connection::connect_to_env()?, STARTUP_TIMEOUT).await
    }

    /// Binds the globals, then waits up to `timeout` each for the keymap
    /// and the output layout.
    async fn start(
        conn: Connection,
        timeout: Duration,
    ) -> Result<Self, WlrootsEmulationCreationError> {
        let mut emulate = Self::bind(&conn)?;
        emulate
            .dispatch_until(timeout, "keymap", |s| s.keymap.is_some())
            .await?;
        // output geometry: requested before this sync, so it has arrived
        // once the sync is answered
        conn.display().sync(&emulate.state.qh, ());
        emulate
            .dispatch_until(timeout, "output layout", |s| s.synced)
            .await?;
        Ok(emulate)
    }

    fn bind(conn: &Connection) -> Result<Self, WlrootsEmulationCreationError> {
        let (globals, queue) = registry_queue_init::<State>(conn)?;
        let qh = queue.handle();

        let seat: wl_seat::WlSeat = globals
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let vpm: VpManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wlr-virtual-pointer-unstable-v1"))?;
        let vkm: VkManager = globals
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "virtual-keyboard-unstable-v1"))?;

        let xdg_output_manager: Option<XdgOutputManager> = globals.bind(&qh, 1..=3, ()).ok();
        if xdg_output_manager.is_none() {
            log::warn!("no xdg-output-manager: the pointer is not placed where it enters");
        }

        let mut emulate = WlrootsEmulation {
            conn: conn.clone(),
            last_flush_failed: false,
            stalled: false,
            state: State {
                keymap: None,
                layout: Layout::default(),
                keyboard: None,
                input_for_client: HashMap::new(),
                seat,
                vpm,
                vkm,
                qh,
                xdg_output_manager,
                outputs: HashMap::new(),
                synced: false,
            },
            queue,
        };
        for global in globals.contents().clone_list() {
            if global.interface == WlOutput::interface().name {
                emulate
                    .state
                    .add_output(globals.registry(), global.name, global.version);
            }
        }
        Ok(emulate)
    }

    /// Dispatches events until `done` holds; a creation error when the
    /// compositor sends nothing that makes it hold within `timeout`.
    async fn dispatch_until(
        &mut self,
        timeout: Duration,
        awaited: &'static str,
        done: impl Fn(&State) -> bool,
    ) -> Result<(), WlrootsEmulationCreationError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            self.queue.dispatch_pending(&mut self.state)?;
            if done(&self.state) {
                return Ok(());
            }
            self.queue.flush()?;
            // `None`: events are queued already
            let Some(guard) = self.queue.prepare_read() else {
                continue;
            };
            let readable = async {
                let fd = AsyncFd::with_interest(guard.connection_fd(), Interest::READABLE)?;
                fd.readable().await.map(drop)
            };
            match tokio::time::timeout_at(deadline, readable).await {
                Ok(readable) => readable?,
                Err(_) => {
                    return Err(WlrootsEmulationCreationError::Timeout {
                        awaited,
                        after: timeout,
                    });
                }
            }
            match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                r => {
                    r?;
                }
            }
        }
    }

    /// Handles what the compositor sent since the last call (keymap and
    /// output changes) without blocking.
    fn read_events(&mut self) -> Result<(), EmulationError> {
        if let Some(guard) = self.queue.prepare_read() {
            match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {}
                r => {
                    r?;
                }
            }
        }
        self.queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| match e {
                DispatchError::Backend(e) => EmulationError::Wayland(e),
                e => io::Error::other(e.to_string()).into(),
            })?;
        Ok(())
    }

    /// Waits until the compositor has read everything queued, at most
    /// `RELEASE_TIMEOUT`.
    async fn drain(&mut self) -> Result<(), EmulationError> {
        let backend = self.conn.backend();
        let fd = AsyncFd::with_interest(backend.poll_fd(), Interest::WRITABLE)?;
        let deadline = tokio::time::Instant::now() + RELEASE_TIMEOUT;
        loop {
            let Ok(writable) = tokio::time::timeout_at(deadline, fd.writable()).await else {
                self.stalled = true;
                return Err(EmulationError::Stalled(RELEASE_TIMEOUT));
            };
            let mut writable = writable?;
            match self.queue.flush() {
                Ok(()) => {
                    self.last_flush_failed = false;
                    return Ok(());
                }
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    writable.clear_ready()
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Moves the pointer of `handle` to the point `place` picks on the
    /// displays and returns it, `None` if the pointer was not moved.
    fn move_pointer(
        &mut self,
        handle: EmulationHandle,
        place: impl FnOnce(&[Rect]) -> Option<(i32, i32)>,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        self.read_events()?;

        let Some(virtual_input) = self.state.input_for_client.get(&handle) else {
            return Ok(None);
        };
        if self.last_flush_failed {
            match self.queue.flush() {
                // outgoing buffer still full, like `consume`: drop the move
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    log::warn!("can't keep up, discarding pointer move ({handle})");
                    return Ok(None);
                }
                _ => {}
            }
        }
        let displays = self.state.displays();
        let (Some((x, y)), Some(layout)) = (place(&displays), bounds(&displays)) else {
            return Ok(None);
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        // absolute motion is relative to the bounding box of the layout
        virtual_input.pointer.motion_absolute(
            now,
            (x - layout.x) as u32,
            (y - layout.y) as u32,
            layout.width as u32,
            layout.height as u32,
        );
        virtual_input.pointer.frame();
        match self.queue.flush() {
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                self.last_flush_failed = true;
                log::warn!("can't keep up, pointer move ({handle}) delayed");
            }
            Ok(()) => self.last_flush_failed = false,
            Err(e) => Err(e)?,
        }
        Ok(Some((x, y)))
    }
}

impl State {
    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());
        let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());

        // TODO: use server side keymap
        if let Some(keymap) = self.keymap.as_ref() {
            keyboard.keymap(keymap.format, keymap.fd.as_fd(), keymap.size);
        } else {
            panic!("no keymap");
        }

        let vinput = VirtualInput {
            pointer,
            keyboard,
            modifiers: Arc::new(Mutex::new(XMods::empty())),
        };

        self.input_for_client.insert(client, vinput);
    }

    fn add_output(&mut self, registry: &wl_registry::WlRegistry, name: u32, version: u32) {
        let Some(manager) = self.xdg_output_manager.as_ref() else {
            return;
        };
        let wl_output: WlOutput = registry.bind(name, version.min(4), &self.qh, ());
        let xdg_output = manager.get_xdg_output(&wl_output, &self.qh, name);
        let output = Output {
            wl_output,
            xdg_output,
            position: (0, 0),
            size: (0, 0),
        };
        self.outputs.insert(name, output);
    }

    fn remove_output(&mut self, name: u32) {
        if let Some(output) = self.outputs.remove(&name) {
            output.xdg_output.destroy();
            if output.wl_output.version() >= 3 {
                output.wl_output.release();
            }
        }
    }

    fn displays(&self) -> Vec<Rect> {
        self.outputs
            .values()
            .map(|o| Rect {
                x: o.position.0,
                y: o.position.1,
                width: o.size.0,
                height: o.size.1,
            })
            .collect()
    }

    fn destroy_client(&mut self, handle: EmulationHandle) {
        if let Some(input) = self.input_for_client.remove(&handle) {
            input.pointer.destroy();
            input.keyboard.destroy();
        }
    }
}

#[async_trait]
impl Emulation for WlrootsEmulation {
    async fn consume(
        &mut self,
        event: Event,
        handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        if self.stalled {
            return Err(EmulationError::Stalled(RELEASE_TIMEOUT));
        }
        self.read_events()?;
        if !self.state.input_for_client.contains_key(&handle) {
            return Ok(());
        }
        if self.last_flush_failed {
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    /*
                     * outgoing buffer is full - sending more events
                     * will overwhelm the output buffer and leave the
                     * wayland connection in a broken state
                     */
                    if !is_release(event) {
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    // a discarded release would leave the key or button held
                    self.drain().await?;
                }
                _ => {}
            }
        }
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            virtual_input.consume_event(event)?;
        }
        match self.queue.flush() {
            // queued, sent with the next flush
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                self.last_flush_failed = true;
                log::warn!("can't keep up, event ({handle}) delayed");
            }
            Ok(()) => self.last_flush_failed = false,
            Err(e) => Err(e)?,
        }
        Ok(())
    }

    async fn ready(&mut self) -> Result<(), EmulationError> {
        if self.stalled {
            return Err(EmulationError::Stalled(RELEASE_TIMEOUT));
        }
        match self.last_flush_failed {
            true => self.drain().await,
            false => Ok(()),
        }
    }

    fn enter_at(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        self.move_pointer(handle, |displays| entry_point(displays, edge, fraction))
    }

    fn text_support(&self) -> TextSupport {
        TextSupport::Layout
    }

    fn desktop(&self) -> Option<Vec<Rect>> {
        let displays = self.state.displays();
        bounds(&displays).is_some().then_some(displays)
    }

    fn pointer_to(
        &mut self,
        handle: EmulationHandle,
        x: i32,
        y: i32,
    ) -> Result<bool, EmulationError> {
        Ok(self.move_pointer(handle, |_| Some((x, y)))?.is_some())
    }

    fn keystroke(&self, c: char) -> Option<Keystroke> {
        self.state.layout.keystroke(c)
    }

    async fn create(&mut self, handle: EmulationHandle) {
        if let Err(e) = self.read_events() {
            log::warn!("{e}");
        }
        self.state.add_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn destroy(&mut self, handle: EmulationHandle) {
        self.state.destroy_client(handle);
        if let Err(e) = self.queue.flush() {
            log::error!("{e}");
        }
    }
    async fn terminate(&mut self) {
        /* nothing to do */
    }
}

/// a key-up or button-up
fn is_release(event: Event) -> bool {
    matches!(
        event,
        Event::Keyboard(KeyboardEvent::Key { state: 0, .. })
            | Event::Pointer(PointerEvent::Button { state: 0, .. })
    )
}

struct VirtualInput {
    pointer: Vp,
    keyboard: Vk,
    modifiers: Arc<Mutex<XMods>>,
}

impl VirtualInput {
    fn consume_event(&self, event: Event) -> Result<(), EmulationError> {
        let now: u32 = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;

        match event {
            Event::Pointer(e) => {
                match e {
                    PointerEvent::Motion { time, dx, dy } => self.pointer.motion(time, dx, dy),
                    PointerEvent::Button {
                        time,
                        button,
                        state,
                    } => {
                        let state: ButtonState = state
                            .try_into()
                            .map_err(|_| EmulationError::InvalidEvent(event))?;
                        self.pointer.button(time, button, state);
                    }
                    PointerEvent::Axis { time, axis, value } => {
                        let axis: Axis = (axis as u32)
                            .try_into()
                            .map_err(|_| EmulationError::InvalidEvent(event))?;
                        self.pointer.axis(time, axis, value);
                        self.pointer.frame();
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let axis: Axis = (axis as u32)
                            .try_into()
                            .map_err(|_| EmulationError::InvalidEvent(event))?;
                        self.pointer
                            .axis_discrete(now, axis, value as f64 / 8., value / 120);
                        self.pointer.axis_source(AxisSource::Wheel);
                        self.pointer.frame();
                    }
                }
                self.pointer.frame();
            }
            Event::Keyboard(e) => match e {
                KeyboardEvent::Key { time, key, state } => {
                    self.keyboard.key(time, key, state as u32);
                    if let Ok(mut mods) = self.modifiers.lock() {
                        if mods.update_by_key_event(key, state) {
                            log::trace!("Key triggers modifier change: {mods:?}");
                            self.keyboard.modifiers(
                                mods.mask_pressed().bits(),
                                0,
                                mods.mask_locks().bits(),
                                0,
                            );
                        }
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed: mods_depressed,
                    latched: mods_latched,
                    locked: mods_locked,
                    group,
                } => {
                    // Synchronize internal modifier state, assuming server is authoritative
                    if let Ok(mut mods) = self.modifiers.lock() {
                        mods.update_by_mods_event(e);
                    }
                    self.keyboard
                        .modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            },
        }
        Ok(())
    }
}

delegate_noop!(State: Vp);
delegate_noop!(State: Vk);
delegate_noop!(State: VpManager);
delegate_noop!(State: VkManager);
delegate_noop!(State: XdgOutputManager);
delegate_noop!(State: ignore WlOutput);

impl Dispatch<WlCallback, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.synced = true;
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut State,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == WlOutput::interface().name => {
                state.add_output(registry, name, version);
            }
            wl_registry::Event::GlobalRemove { name } => state.remove_output(name),
            _ => {}
        }
    }
}

impl Dispatch<XdgOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &XdgOutput,
        event: zxdg_output_v1::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state.outputs.get_mut(name) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => output.position = (x, y),
            zxdg_output_v1::Event::LogicalSize { width, height } => output.size = (width, height),
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: <WlKeyboard as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_keyboard::Event::Keymap { format, fd, size } = event {
            // wlroots sends NO_KEYMAP (/dev/null, size 0) while the active
            // keyboard has none, e.g. a virtual keyboard before its `keymap`
            // request; uploading that ends the connection (`no_memory`)
            if format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) || size == 0 {
                log::debug!("ignoring a keymap event without a keymap");
                return;
            }
            let keymap = match Keymap::read(u32::from(format), fd, size) {
                Ok(keymap) => keymap,
                Err(e) => {
                    log::warn!("could not read the keymap: {e}");
                    return;
                }
            };
            // wlroots sends the keymap to every client whenever it is set,
            // also when a virtual keyboard uploads it: uploading an
            // unchanged keymap again would loop and flood the connection
            if state.keymap.as_ref().is_some_and(|k| k.text == keymap.text) {
                return;
            }
            // keyboards made before the change would type the old layout
            for input in state.input_for_client.values() {
                input
                    .keyboard
                    .keymap(keymap.format, keymap.fd.as_fd(), keymap.size);
            }
            state.layout = Layout::new(&keymap.text).unwrap_or_default();
            state.keymap = Some(keymap);
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qhandle: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if !capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(keyboard) = state.keyboard.take() {
                    keyboard.release();
                }
            } else if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qhandle, ()));
            }
        }
    }
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}

impl XMods {
    fn update_by_mods_event(&mut self, evt: KeyboardEvent) {
        if let KeyboardEvent::Modifiers {
            depressed, locked, ..
        } = evt
        {
            *self = XMods::from_bits_truncate(depressed) | XMods::from_bits_truncate(locked);
        }
    }

    fn update_by_key_event(&mut self, key: u32, state: u8) -> bool {
        if let Ok(key) = scancode::Linux::try_from(key) {
            log::trace!("Attempting to process modifier from: {key:#?}");
            let pressed_mask = match key {
                scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
                scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
                scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => XMods::Mod1Mask,
                scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
                _ => XMods::empty(),
            };

            let locked_mask = match key {
                scancode::Linux::KeyCapsLock => XMods::LockMask,
                scancode::Linux::KeyNumlock => XMods::Mod2Mask,
                scancode::Linux::KeyScrollLock => XMods::Mod3Mask,
                _ => XMods::empty(),
            };

            // unchanged
            if pressed_mask.is_empty() && locked_mask.is_empty() {
                log::trace!("{key:#?} is not a modifier key");
                return false;
            }
            match state {
                1 => self.insert(pressed_mask),
                _ => {
                    self.remove(pressed_mask);
                    self.toggle(locked_mask);
                }
            }
            true
        } else {
            false
        }
    }

    fn mask_locks(&self) -> XMods {
        *self & (XMods::LockMask | XMods::Mod2Mask | XMods::Mod3Mask)
    }

    fn mask_pressed(&self) -> XMods {
        *self & (XMods::ShiftMask | XMods::ControlMask | XMods::Mod1Mask | XMods::Mod4Mask)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{Backend, InputEmulation, InputEmulationError, Selected};
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// the globals the backend binds; the seat has no keyboard
    const GLOBALS: &[(&str, u32)] = &[
        ("wl_seat", 7),
        ("zwlr_virtual_pointer_manager_v1", 1),
        ("zwp_virtual_keyboard_manager_v1", 1),
        ("zxdg_output_manager_v1", 3),
    ];

    /// a Wayland message: object, opcode and the encoded arguments
    fn message(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
        let size = (8 + args.len()) as u32;
        let mut msg = object.to_ne_bytes().to_vec();
        msg.extend((size << 16 | u32::from(opcode)).to_ne_bytes());
        msg.extend(args);
        msg
    }

    /// a `wl_registry.global` event
    fn global(registry: u32, name: u32, interface: &str, version: u32) -> Vec<u8> {
        let mut args = name.to_ne_bytes().to_vec();
        // a string: length with the NUL, padded to 4 bytes
        args.extend((interface.len() as u32 + 1).to_ne_bytes());
        args.extend(interface.as_bytes());
        args.resize(args.len() + 4 - interface.len() % 4, 0);
        args.extend(version.to_ne_bytes());
        message(registry, 0, &args)
    }

    /// a request: object, opcode and the encoded arguments
    type Request = (u32, u32, Vec<u8>);

    /// The compositor end of a fake connection: it announces `GLOBALS` and
    /// answers `wl_display.sync`, anything else the test sends. It records
    /// every request.
    struct Compositor {
        stream: Arc<Mutex<UnixStream>>,
        /// the client's `wl_registry`
        registry: Arc<AtomicU32>,
        requests: Arc<Mutex<Vec<Request>>>,
        /// reads no further request while set, as a busy compositor
        paused: Arc<AtomicBool>,
    }

    impl Compositor {
        fn start() -> (Connection, Self) {
            let (client, server) = UnixStream::pair().unwrap();
            let mut reader = server.try_clone().unwrap();
            let compositor = Self {
                stream: Arc::new(Mutex::new(server)),
                registry: Arc::default(),
                requests: Arc::default(),
                paused: Arc::default(),
            };
            let (stream, registry, requests, paused) = (
                compositor.stream.clone(),
                compositor.registry.clone(),
                compositor.requests.clone(),
                compositor.paused.clone(),
            );
            std::thread::spawn(move || {
                let mut header = [0; 8];
                // ends when the client hangs up
                loop {
                    while paused.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    if reader.read_exact(&mut header).is_err() {
                        return;
                    }
                    let word = |b: &[u8]| u32::from_ne_bytes(b[..4].try_into().unwrap());
                    let (object, size_opcode) = (word(&header), word(&header[4..]));
                    let mut args = vec![0; (size_opcode >> 16) as usize - 8];
                    if reader.read_exact(&mut args).is_err() {
                        return;
                    }
                    requests
                        .lock()
                        .unwrap()
                        .push((object, size_opcode & 0xffff, args.clone()));
                    // both requests carry the new object's id first
                    let id = || word(&args);
                    let reply = match (object, size_opcode & 0xffff) {
                        // wl_display.sync: wl_callback.done
                        (1, 0) => message(id(), 0, &0u32.to_ne_bytes()),
                        // wl_display.get_registry
                        (1, 1) => {
                            registry.store(id(), Ordering::SeqCst);
                            (1..)
                                .zip(GLOBALS)
                                .flat_map(|(name, &(interface, version))| {
                                    global(id(), name, interface, version)
                                })
                                .collect()
                        }
                        _ => continue,
                    };
                    if stream.lock().unwrap().write_all(&reply).is_err() {
                        return;
                    }
                }
            });
            (Connection::from_socket(client).unwrap(), compositor)
        }

        /// announces a new global
        fn announce(&self, name: u32, interface: &str, version: u32) {
            let registry = self.registry.load(Ordering::SeqCst);
            let event = global(registry, name, interface, version);
            self.stream.lock().unwrap().write_all(&event).unwrap();
        }

        /// how many `opcode` requests `object` received
        fn received(&self, object: &impl Proxy, opcode: u32) -> usize {
            let id = object.id().protocol_id();
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .filter(|(o, op, _)| (*o, *op) == (id, opcode))
                .count()
        }

        /// how many `zwp_virtual_keyboard_v1.key` requests released `key`:
        /// the only requests with opcode 1 and the arguments time, key, state
        fn key_ups(&self, key: u32) -> usize {
            let up = [0, key, 0].map(u32::to_ne_bytes);
            let requests = self.requests.lock().unwrap();
            requests
                .iter()
                .filter(|(_, opcode, args)| {
                    *opcode == 1 && args.len() == 12 && args[4..] == up[1..].concat()
                })
                .count()
        }

        /// stops reading requests, so the client's socket buffer fills
        fn pause(&self) {
            self.paused.store(true, Ordering::SeqCst);
        }
    }

    impl Drop for Compositor {
        /// lets the reader see the client hang up
        fn drop(&mut self) {
            self.paused.store(false, Ordering::SeqCst);
        }
    }

    /// far more motion than a Unix socket buffer holds (`wmem_default`,
    /// 208 KiB by default), so the backend falls behind a paused compositor
    const FILL: usize = 1 << 16;

    fn key(key: u32, state: u8) -> Event {
        Event::Keyboard(KeyboardEvent::Key {
            time: 0,
            key,
            state,
        })
    }

    const MOTION: Event = Event::Pointer(PointerEvent::Motion {
        time: 0,
        dx: 1.,
        dy: 0.,
    });

    const KEY_A: u32 = 30;

    /// A key-up that meets a full connection waits for the compositor to
    /// read instead of being discarded: discarded, it counted as emulated,
    /// so the key stayed held in the compositor and `destroy` did not
    /// repeat it.
    #[tokio::test]
    async fn a_release_reaches_a_compositor_that_fell_behind() {
        let (conn, compositor) = Compositor::start();
        let selected = Selected {
            backend: Backend::Wlroots,
            auto: false,
        };
        let mut emulation = InputEmulation::with_emulation(Box::new(bound(&conn)), selected);
        emulation.create(1).await;
        emulation.consume(key(KEY_A, 1), 1).await.unwrap();
        compositor.pause();
        for _ in 0..FILL {
            emulation.consume(MOTION, 1).await.unwrap();
        }
        let paused = compositor.paused.clone();
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            paused.store(false, Ordering::SeqCst);
        });
        emulation.consume(key(KEY_A, 0), 1).await.unwrap();
        resume.join().unwrap();
        // the compositor reads everything queued
        loop {
            match conn.flush() {
                Ok(()) => break,
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(1)).await
                }
                Err(e) => panic!("{e}"),
            }
        }
        emulation.destroy(1).await;
        conn.roundtrip().unwrap();
        assert_eq!(compositor.key_ups(KEY_A), 1);
    }

    /// A compositor that stops reading fails a key-up after
    /// `RELEASE_TIMEOUT` with a transient error, so the backend is
    /// recreated; later events fail at once instead of waiting again.
    #[tokio::test]
    async fn a_release_fails_when_the_compositor_stops_reading() {
        use futures::FutureExt;
        let (conn, compositor) = Compositor::start();
        let mut emulation = bound(&conn);
        emulation.create(1).await;
        emulation.consume(key(KEY_A, 1), 1).await.unwrap();
        compositor.pause();
        for _ in 0..FILL {
            emulation.consume(MOTION, 1).await.unwrap();
        }
        let error = emulation.consume(key(KEY_A, 0), 1).await.unwrap_err();
        assert!(InputEmulationError::from(error).is_transient());
        let again = emulation.consume(key(KEY_A, 0), 1).now_or_never();
        assert!(matches!(again, Some(Err(EmulationError::Stalled(_)))));
    }

    /// a keymap fd holding `text`, as the compositor sends it
    fn keymap_fd(text: &str) -> OwnedFd {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!("mimouse-keymap-{}-{n}", std::process::id()));
        std::fs::write(&path, text).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        file.into()
    }

    /// a `wl_keyboard.keymap` event carrying `text`
    fn keymap_event(text: &str) -> wl_keyboard::Event {
        wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
            fd: keymap_fd(text),
            size: text.len() as u32,
        }
    }

    const KEYMAP: &str = "xkb_keymap { us };";

    /// a backend bound on `conn`, with the keymap `KEYMAP`: the fake cannot
    /// send one, that needs fd passing
    fn bound(conn: &Connection) -> WlrootsEmulation {
        let mut emulation = WlrootsEmulation::bind(conn).unwrap();
        let qh = emulation.queue.handle();
        let keyboard = WlKeyboard::inert(conn.backend().downgrade());
        <State as Dispatch<WlKeyboard, ()>>::event(
            &mut emulation.state,
            &keyboard,
            keymap_event(KEYMAP),
            &(),
            conn,
            &qh,
        );
        assert!(emulation.state.keymap.is_some());
        emulation
    }

    /// a backend on a fake compositor without outputs
    pub(crate) fn on_fake_compositor() -> WlrootsEmulation {
        bound(&Compositor::start().0)
    }

    #[tokio::test]
    async fn a_seat_without_keyboard_fails_in_time() {
        let (conn, _compositor) = Compositor::start();
        let result = WlrootsEmulation::start(conn, Duration::from_millis(200)).await;
        assert!(matches!(
            result,
            Err(WlrootsEmulationCreationError::Timeout {
                awaited: "keymap",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn compositor_changes_arrive_with_input() {
        let (conn, compositor) = Compositor::start();
        let mut emulation = bound(&conn);
        emulation.create(1).await;
        assert!(emulation.state.outputs.is_empty());

        // a monitor plugged in while a peer only sends input, no `enter_at`
        compositor.announce(GLOBALS.len() as u32 + 1, "wl_output", 4);
        let motion = PointerEvent::Motion {
            time: 0,
            dx: 1.,
            dy: 0.,
        };
        emulation.consume(Event::Pointer(motion), 1).await.unwrap();
        assert_eq!(emulation.state.outputs.len(), 1);
    }

    /// wlroots sends `NO_KEYMAP` (/dev/null, size 0) while the seat's active
    /// keyboard has no keymap, e.g. between a virtual keyboard's creation and
    /// its `keymap` request. Uploading that to a virtual keyboard makes the
    /// compositor end the connection (`no_memory`), so it must not replace
    /// the keymap later keyboards get.
    #[test]
    fn a_no_keymap_event_keeps_the_last_keymap() {
        let (conn, _compositor) = Compositor::start();
        let mut emulation = bound(&conn);
        let keyboard = WlKeyboard::inert(conn.backend().downgrade());
        let devnull = std::fs::File::open("/dev/null").unwrap();
        let event = wl_keyboard::Event::Keymap {
            format: WEnum::Value(wl_keyboard::KeymapFormat::NoKeymap),
            fd: devnull.into(),
            size: 0,
        };
        let qh = emulation.queue.handle();
        <State as Dispatch<WlKeyboard, ()>>::event(
            &mut emulation.state,
            &keyboard,
            event,
            &(),
            &conn,
            &qh,
        );
        let keymap = emulation.state.keymap.as_ref().expect("keymap");
        assert_eq!(keymap.text, KEYMAP.as_bytes());
    }

    /// wlroots sends the keymap to every client whenever the seat's keymap
    /// is set, also when a virtual keyboard uploads one. Uploading an
    /// unchanged keymap again then makes it send it again: a loop that
    /// floods the connection until input events are discarded.
    #[tokio::test]
    async fn an_unchanged_keymap_is_not_uploaded_again() {
        let (conn, compositor) = Compositor::start();
        let mut emulation = bound(&conn);
        emulation.create(1).await;
        let qh = emulation.queue.handle();
        let keyboard = WlKeyboard::inert(conn.backend().downgrade());
        let mut keymap = |text| {
            <State as Dispatch<WlKeyboard, ()>>::event(
                &mut emulation.state,
                &keyboard,
                keymap_event(text),
                &(),
                &conn,
                &qh,
            );
            emulation.queue.roundtrip(&mut emulation.state).unwrap();
            let virtual_keyboard = &emulation.state.input_for_client[&1].keyboard;
            compositor.received(virtual_keyboard, 0)
        };
        // the upload when the keyboard was made
        assert_eq!(keymap(KEYMAP), 1, "unchanged keymap uploaded again");
        assert_eq!(keymap("xkb_keymap { de };"), 2, "a new layout is uploaded");
    }

    /// The seat announces its capabilities again whenever a keyboard is
    /// added or removed. Each keyboard gets every keymap event, so the
    /// backend keeps exactly one while the seat has a keyboard.
    #[test]
    fn the_seat_keyboard_is_requested_once() {
        let (conn, compositor) = Compositor::start();
        let mut emulation = bound(&conn);
        let qh = emulation.queue.handle();
        let seat = emulation.state.seat.clone();
        let mut announce = |capabilities| {
            <State as Dispatch<WlSeat, ()>>::event(
                &mut emulation.state,
                &seat,
                wl_seat::Event::Capabilities {
                    capabilities: WEnum::Value(capabilities),
                },
                &(),
                &conn,
                &qh,
            );
            emulation.queue.roundtrip(&mut emulation.state).unwrap();
            compositor.received(&seat, 1)
        };
        assert_eq!(announce(wl_seat::Capability::Keyboard), 1);
        assert_eq!(announce(wl_seat::Capability::Keyboard), 1);
        // removed and added again: a new keyboard
        assert_eq!(announce(wl_seat::Capability::empty()), 1);
        assert_eq!(announce(wl_seat::Capability::Keyboard), 2);
    }
}
