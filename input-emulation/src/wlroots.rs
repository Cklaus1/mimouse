use crate::error::EmulationError;

use super::{Emulation, error::WlrootsEmulationCreationError};
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
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{wl_registry, wl_seat},
};

use input_event::{Event, KeyboardEvent, PointerEvent, scancode};

use super::error::WaylandBindError;
use super::geometry::{Rect, entry_point};
use super::{Edge, EmulationHandle};

/// How long the compositor may take to send the keymap, and then the output
/// layout. A seat without a keyboard never sends a keymap; creation fails
/// then and is retried later.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

struct State {
    keymap: Option<(u32, OwnedFd, u32)>,
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
    last_flush_failed: bool,
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
            last_flush_failed: false,
            state: State {
                keymap: None,
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
}

impl State {
    fn add_client(&mut self, client: EmulationHandle) {
        let pointer: Vp = self.vpm.create_virtual_pointer(None, &self.qh, ());
        let keyboard: Vk = self.vkm.create_virtual_keyboard(&self.seat, &self.qh, ());

        // TODO: use server side keymap
        if let Some((format, fd, size)) = self.keymap.as_ref() {
            keyboard.keymap(*format, fd.as_fd(), *size);
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
        if let Some(virtual_input) = self.state.input_for_client.get(&handle) {
            if self.last_flush_failed {
                match self.queue.flush() {
                    Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                        /*
                         * outgoing buffer is full - sending more events
                         * will overwhelm the output buffer and leave the
                         * wayland connection in a broken state
                         */
                        log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                        return Ok(());
                    }
                    _ => {}
                }
            }
            virtual_input.consume_event(event)?;
            match self.queue.flush() {
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.last_flush_failed = true;
                    log::warn!("can't keep up, discarding event: ({handle}) - {event:?}");
                }
                Ok(()) => self.last_flush_failed = false,
                Err(e) => Err(e)?,
            }
        }
        Ok(())
    }

    fn enter_at(
        &mut self,
        handle: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        // pick up output changes without blocking
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
            .map_err(|e| io::Error::other(e.to_string()))?;

        let Some(virtual_input) = self.state.input_for_client.get(&handle) else {
            return Ok(None);
        };
        if self.last_flush_failed {
            match self.queue.flush() {
                // outgoing buffer still full, like `consume`: drop the move
                Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                    log::warn!("can't keep up, discarding entry point ({handle})");
                    return Ok(None);
                }
                _ => {}
            }
        }
        let displays = self.state.displays();
        let Some((x, y)) = entry_point(&displays, edge, fraction) else {
            return Ok(None);
        };
        // absolute motion is relative to the bounding box of the layout
        let left = displays.iter().map(|d| d.x).min().unwrap_or_default();
        let top = displays.iter().map(|d| d.y).min().unwrap_or_default();
        let right = displays
            .iter()
            .map(|d| d.x + d.width)
            .max()
            .unwrap_or_default();
        let bottom = displays
            .iter()
            .map(|d| d.y + d.height)
            .max()
            .unwrap_or_default();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u32;
        virtual_input.pointer.motion_absolute(
            now,
            (x - left) as u32,
            (y - top) as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        );
        virtual_input.pointer.frame();
        match self.queue.flush() {
            Err(WaylandError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => {
                self.last_flush_failed = true;
                log::warn!("can't keep up, entry point ({handle}) delayed");
            }
            Ok(()) => self.last_flush_failed = false,
            Err(e) => Err(e)?,
        }
        Ok(Some((x, y)))
    }

    async fn create(&mut self, handle: EmulationHandle) {
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
            state.keymap = Some((u32::from(format), fd, size));
        }
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
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
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qhandle, ());
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
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    /// the globals the backend needs; the seat has no keyboard
    const GLOBALS: &[(&str, u32)] = &[
        ("wl_seat", 7),
        ("zwlr_virtual_pointer_manager_v1", 1),
        ("zwp_virtual_keyboard_manager_v1", 1),
    ];

    /// a Wayland message: object, opcode and the encoded arguments
    fn message(object: u32, opcode: u16, args: &[u8]) -> Vec<u8> {
        let size = (8 + args.len()) as u32;
        let mut msg = object.to_ne_bytes().to_vec();
        msg.extend((size << 16 | u32::from(opcode)).to_ne_bytes());
        msg.extend(args);
        msg
    }

    /// a Wayland string argument: length with the NUL, padded to 4 bytes
    fn string(s: &str) -> Vec<u8> {
        let mut arg = (s.len() as u32 + 1).to_ne_bytes().to_vec();
        arg.extend(s.as_bytes());
        arg.push(0);
        arg.resize(arg.len().next_multiple_of(4), 0);
        arg
    }

    /// A compositor that announces `GLOBALS` and answers `wl_display.sync`,
    /// and sends nothing else.
    fn fake_compositor() -> Connection {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            let mut header = [0; 8];
            // ends when the client hangs up
            while server.read_exact(&mut header).is_ok() {
                let word = |b: &[u8]| u32::from_ne_bytes(b[..4].try_into().unwrap());
                let (object, size_opcode) = (word(&header), word(&header[4..]));
                let mut args = vec![0; (size_opcode >> 16) as usize - 8];
                if server.read_exact(&mut args).is_err() {
                    return;
                }
                let reply = match (object, size_opcode & 0xffff) {
                    // wl_display.sync(callback): wl_callback.done
                    (1, 0) => message(word(&args), 0, &0u32.to_ne_bytes()),
                    // wl_display.get_registry: wl_registry.global per global
                    (1, 1) => (1u32..)
                        .zip(GLOBALS)
                        .flat_map(|(name, (interface, version))| {
                            let mut global = name.to_ne_bytes().to_vec();
                            global.extend(string(interface));
                            global.extend(version.to_ne_bytes());
                            message(word(&args), 0, &global)
                        })
                        .collect(),
                    _ => continue,
                };
                if server.write_all(&reply).is_err() {
                    return;
                }
            }
        });
        Connection::from_socket(client).unwrap()
    }

    #[tokio::test]
    async fn a_seat_without_keyboard_fails_in_time() {
        let timeout = Duration::from_millis(200);
        let result = WlrootsEmulation::start(fake_compositor(), timeout).await;
        assert!(matches!(
            result,
            Err(WlrootsEmulationCreationError::Timeout {
                awaited: "keymap",
                ..
            })
        ));
    }
}
