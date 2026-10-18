use async_trait::async_trait;
use futures_core::Stream;
use std::{
    collections::{HashMap, VecDeque},
    env,
    fmt::{self, Display},
    io::{self, ErrorKind},
    os::fd::{AsFd, RawFd},
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::unix::AsyncFd;

use std::{
    fs::File,
    io::{BufWriter, Write},
    os::unix::prelude::AsRawFd,
    sync::Arc,
};

use wayland_protocols::{
    wp::{
        keyboard_shortcuts_inhibit::zv1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        },
        pointer_constraints::zv1::client::{
            zwp_locked_pointer_v1::ZwpLockedPointerV1,
            zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
        },
        relative_pointer::zv1::client::{
            zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
            zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
        },
    },
    xdg::xdg_output::zv1::client::{
        zxdg_output_manager_v1::ZxdgOutputManagerV1,
        zxdg_output_v1::{self, ZxdgOutputV1},
    },
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{Global, GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region,
        wl_registry::{self, WlRegistry},
        wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{
    CaptureError, CaptureEvent, EdgeRange,
    geometry::{self, Rect},
};

use super::{
    Capture, Position,
    error::{LayerShellCaptureCreationError, WaylandBindError},
};

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Clone, Debug)]
struct Output {
    wl_output: WlOutput,
    global: Global,
    info: Option<OutputInfo>,
    pending_info: OutputInfo,
    has_xdg_info: bool,
}

impl Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(info) = &self.info {
            write!(
                f,
                "{} {}x{} @pos {:?} ({})",
                info.name, info.size.0, info.size.1, info.position, info.description
            )
        } else {
            write!(f, "unknown output")
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OutputInfo {
    description: String,
    name: String,
    position: (i32, i32),
    size: (i32, i32),
}

impl OutputInfo {
    /// the output in desktop (logical) coordinates
    fn rect(&self) -> Rect {
        Rect {
            x: f64::from(self.position.0),
            y: f64::from(self.position.1),
            width: f64::from(self.size.0),
            height: f64::from(self.size.1),
        }
    }
}

struct State {
    /// barrier positions and the part of the edge each covers
    active_positions: HashMap<Position, Option<EdgeRange>>,
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    active_windows: Vec<Arc<Window>>,
    focused: Option<Arc<Window>>,
    global_list: GlobalList,
    globals: Globals,
    wayland_fd: RawFd,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<(Position, CaptureEvent)>,
    outputs: Vec<Output>,
    scroll_discrete_pending: bool,
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.state.wayland_fd
    }
}

pub struct LayerShellInputCapture(AsyncFd<Inner>);

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
    pos: Position,
    /// desktop (logical) coordinates of the surface's top left corner
    origin: (f64, f64),
}

impl Window {
    /// a surface on `output`'s `pos` side covering `[first, end)` along it
    /// (desktop coordinates)
    fn new(
        state: &State,
        qh: &QueueHandle<State>,
        output: &WlOutput,
        pos: Position,
        info: &OutputInfo,
        (first, end): (i32, i32),
    ) -> Window {
        let size = info.size;
        log::debug!("creating window output: {output:?}, size: {size:?}, along: {first}..{end}");
        let g = &state.globals;

        let length = (end - first) as u32;
        let (width, height) = match pos {
            Position::Left | Position::Right => (1, length),
            Position::Top | Position::Bottom => (length, 1),
        };
        let (x, y) = info.position;
        let origin = match pos {
            Position::Left => (x, first),
            Position::Right => (x + size.0 - 1, first),
            Position::Top => (first, y),
            Position::Bottom => (first, y + size.1 - 1),
        };
        let origin = (f64::from(origin.0), f64::from(origin.1));
        let mut file = tempfile::tempfile().unwrap();
        draw(&mut file, (width, height));
        let pool = g
            .shm
            .create_pool(file.as_fd(), (width * height * 4) as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            qh,
            (),
        );
        let surface = g.compositor.create_surface(qh, ());

        let layer_surface = g.layer_shell.get_layer_surface(
            &surface,
            Some(output),
            Layer::Overlay,
            "MiMouse Sharing".into(),
            qh,
            (),
        );
        // anchored to the edge and the top / left corner, so the margin
        // places the surface at the start of the range
        let (anchor, top, left) = match pos {
            Position::Left => (Anchor::Left | Anchor::Top, first - y, 0),
            Position::Right => (Anchor::Right | Anchor::Top, first - y, 0),
            Position::Top => (Anchor::Top | Anchor::Left, 0, first - x),
            Position::Bottom => (Anchor::Bottom | Anchor::Left, 0, first - x),
        };

        layer_surface.set_anchor(anchor);
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_margin(top, 0, 0, left);
        surface.set_input_region(None);
        surface.commit();
        Window {
            pos,
            buffer,
            surface,
            layer_surface,
            origin,
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        log::debug!("destroying window!");
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

fn get_edges(outputs: &[Output], pos: Position) -> Vec<(Output, i32)> {
    outputs
        .iter()
        .filter_map(|output| {
            output.info.as_ref().map(|info| {
                (
                    output.clone(),
                    match pos {
                        Position::Left => info.position.0,
                        Position::Right => info.position.0 + info.size.0,
                        Position::Top => info.position.1,
                        Position::Bottom => info.position.1 + info.size.1,
                    },
                )
            })
        })
        .collect()
}

fn get_output_configuration(state: &State, pos: Position) -> Vec<Output> {
    // get all output edges corresponding to the position
    let edges = get_edges(&state.outputs, pos);
    let opposite_edges = get_edges(&state.outputs, pos.opposite());

    // remove those edges that are at the same position
    // as an opposite edge of a different output
    edges
        .iter()
        .filter(|(_, edge)| !opposite_edges.iter().map(|(_, e)| *e).any(|e| &e == edge))
        .map(|(o, _)| o.clone())
        .collect()
}

fn draw(f: &mut File, (width, height): (u32, u32)) {
    let mut buf = BufWriter::new(f);
    for _ in 0..height {
        for _ in 0..width {
            if env::var("LM_DEBUG_LAYER_SHELL").ok().is_some() {
                // AARRGGBB
                buf.write_all(&0xff11d116u32.to_ne_bytes()).unwrap();
            } else {
                // AARRGGBB
                buf.write_all(&0x00000000u32.to_ne_bytes()).unwrap();
            }
        }
    }
}

impl LayerShellInputCapture {
    pub fn new() -> std::result::Result<Self, LayerShellCaptureCreationError> {
        Self::bind(Connection::connect_to_env()?)
    }

    /// a backend on `conn` (tests pass a fake compositor's)
    fn bind(conn: Connection) -> std::result::Result<Self, LayerShellCaptureCreationError> {
        let (global_list, mut queue) = registry_queue_init::<State>(&conn)?;

        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = global_list
            .bind(&qh, 4..=5, ())
            .map_err(|e| WaylandBindError::new(e, "wl_compositor 4..=5"))?;
        let xdg_output_manager: ZxdgOutputManagerV1 = global_list
            .bind(&qh, 1..=3, ())
            .map_err(|e| WaylandBindError::new(e, "xdg_output_manager 1..=3"))?;
        let shm: wl_shm::WlShm = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "wl_shm"))?;
        let layer_shell: ZwlrLayerShellV1 = global_list
            .bind(&qh, 3..=4, ())
            .map_err(|e| WaylandBindError::new(e, "wlr_layer_shell 3..=4"))?;
        let seat: wl_seat::WlSeat = global_list
            .bind(&qh, 7..=8, ())
            .map_err(|e| WaylandBindError::new(e, "wl_seat 7..=8"))?;

        let pointer_constraints: ZwpPointerConstraintsV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_pointer_constraints_v1"))?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_relative_pointer_manager_v1"))?;
        let shortcut_inhibit_manager: Result<
            ZwpKeyboardShortcutsInhibitManagerV1,
            WaylandBindError,
        > = global_list
            .bind(&qh, 1..=1, ())
            .map_err(|e| WaylandBindError::new(e, "zwp_keyboard_shortcuts_inhibit_manager_v1"));
        // layer-shell backend still works without this protocol so we make it an optional dependency
        if let Err(e) = &shortcut_inhibit_manager {
            log::warn!("shortcut_inhibit_manager not supported: {e}\nkeybinds handled by the compositor will not be passed
                to the client");
        }
        let shortcut_inhibit_manager = shortcut_inhibit_manager.ok();

        let mut state = State {
            active_positions: Default::default(),
            pointer: None,
            keyboard: None,
            global_list,
            globals: Globals {
                compositor,
                shm,
                layer_shell,
                seat,
                pointer_constraints,
                relative_pointer_manager,
                shortcut_inhibit_manager,
                xdg_output_manager,
            },
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            active_windows: Vec::new(),
            focused: None,
            qh,
            wayland_fd: queue.as_fd().as_raw_fd(),
            read_guard: None,
            pending_events: VecDeque::new(),
            outputs: vec![],
            scroll_discrete_pending: false,
        };

        for global in state.global_list.contents().clone_list() {
            state.register_global(global);
        }

        // flush outgoing events
        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                    continue;
                }
            }
        };
        state.read_guard = Some(read_guard);

        let inner = AsyncFd::new(Inner { queue, state })?;

        Ok(LayerShellInputCapture(inner))
    }

    fn add_client(&mut self, pos: Position, range: Option<EdgeRange>) {
        let state = &mut self.0.get_mut().state;
        state.remove_client(pos);
        state.add_client(pos, range);
    }

    fn delete_client(&mut self, pos: Position) {
        self.0.get_mut().state.remove_client(pos);
    }
}

impl State {
    fn update_output_info(&mut self, name: u32) {
        let output = self
            .outputs
            .iter_mut()
            .find(|o| o.global.name == name)
            .expect("output not found");
        if output.has_xdg_info {
            output.info.replace(output.pending_info.clone());
            self.update_windows();
        }
    }

    fn register_global(&mut self, global: Global) {
        if global.interface.as_str() == "wl_output" {
            log::debug!("new output global: wl_output {}", global.name);
            let wl_output = self.global_list.registry().bind::<WlOutput, _, _>(
                global.name,
                4,
                &self.qh,
                global.name,
            );
            self.globals
                .xdg_output_manager
                .get_xdg_output(&wl_output, &self.qh, global.name);
            self.outputs.push(Output {
                wl_output,
                global,
                info: None,
                has_xdg_info: false,
                pending_info: Default::default(),
            })
        }
    }

    fn deregister_global(&mut self, name: u32) {
        self.outputs.retain(|o| {
            if o.global.name == name {
                log::debug!("{o} (global {:?}) removed", o.global);
                o.wl_output.release();
                false
            } else {
                true
            }
        });
    }

    fn grab(
        &mut self,
        surface: &WlSurface,
        pointer: &WlPointer,
        serial: u32,
        qh: &QueueHandle<State>,
    ) {
        let window = self.focused.as_ref().unwrap();

        // hide the cursor
        pointer.set_cursor(serial, None, 0, 0);

        // capture input
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        window.surface.commit();

        // lock pointer
        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.globals.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                qh,
                (),
            ));
        }

        // request relative input
        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(self.globals.relative_pointer_manager.get_relative_pointer(
                pointer,
                qh,
                (),
            ));
        }

        // capture modifier keys
        if let Some(shortcut_inhibit_manager) = &self.globals.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor = Some(shortcut_inhibit_manager.inhibit_shortcuts(
                    surface,
                    &self.globals.seat,
                    qh,
                    (),
                ));
            }
        }
    }

    fn ungrab(&mut self) {
        // get focused client
        let window = match self.focused.as_ref() {
            Some(focused) => focused,
            None => return,
        };

        // ungrab surface
        window
            .layer_surface
            .set_keyboard_interactivity(KeyboardInteractivity::None);
        window.surface.commit();

        // destroy pointer lock
        if let Some(pointer_lock) = &self.pointer_lock {
            pointer_lock.destroy();
            self.pointer_lock = None;
        }

        // destroy relative input
        if let Some(rel_pointer) = &self.rel_pointer {
            rel_pointer.destroy();
            self.rel_pointer = None;
        }

        // destroy shortcut inhibitor
        if let Some(shortcut_inhibitor) = &self.shortcut_inhibitor {
            shortcut_inhibitor.destroy();
            self.shortcut_inhibitor = None;
        }
    }

    fn add_client(&mut self, pos: Position, range: Option<EdgeRange>) {
        self.active_positions.insert(pos, range);
        let outputs = get_output_configuration(self, pos);

        log::info!(
            "adding capture for position {pos} - using outputs: {:?}",
            outputs
                .iter()
                .map(|o| o
                    .info
                    .as_ref()
                    .map(|i| i.name.to_owned())
                    .unwrap_or("unknown output".to_owned()))
                .collect::<Vec<_>>()
        );
        let displays = self.displays();
        outputs.iter().for_each(|o| {
            let Some(info) = o.info.as_ref() else {
                return;
            };
            // a range shortens the surfaces; outputs it misses get none
            let Some((first, end)) = geometry::barrier_segment(&displays, &info.rect(), pos, range)
            else {
                return;
            };
            let along = (first as i32, end as i32);
            let window = Window::new(self, &self.qh, &o.wl_output, pos, info, along);
            self.active_windows.push(Arc::new(window));
        });
    }

    fn remove_client(&mut self, pos: Position) {
        self.active_positions.remove(&pos);
        // the grab is on the focused window: release it before the window
        // goes, a grab on another edge stays
        if self.focused.as_ref().is_some_and(|w| w.pos == pos) {
            if self.pointer_lock.is_some() {
                self.pending_events.push_back((pos, CaptureEvent::End));
            }
            self.ungrab();
            self.focused = None;
        }
        // remove all windows corresponding to this client
        self.active_windows.retain(|w| w.pos != pos);
    }

    fn update_windows(&mut self) {
        log::info!("active outputs: ");
        for output in self.outputs.iter().filter(|o| o.info.is_some()) {
            log::info!(" * {output}");
        }

        self.active_windows.clear();

        let active_positions = self
            .active_positions
            .iter()
            .map(|(&pos, &range)| (pos, range))
            .collect::<Vec<_>>();
        for (pos, range) in active_positions {
            self.add_client(pos, range);
        }
    }

    /// the outputs, in desktop (logical) coordinates
    fn displays(&self) -> Vec<Rect> {
        self.outputs
            .iter()
            .filter_map(|o| o.info.as_ref())
            .map(OutputInfo::rect)
            .collect()
    }
}

impl Inner {
    fn read(&mut self) -> bool {
        match self.state.read_guard.take().unwrap().read() {
            Ok(_) => true,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => false,
            Err(WaylandError::Io(e)) => {
                log::error!("error reading from wayland socket: {e}");
                false
            }
            Err(WaylandError::Protocol(e)) => {
                panic!("wayland protocol violation: {e}")
            }
        }
    }

    fn prepare_read(&mut self) -> io::Result<()> {
        loop {
            match self.queue.prepare_read() {
                None => match self.queue.dispatch_pending(&mut self.state) {
                    Ok(_) => continue,
                    Err(DispatchError::Backend(WaylandError::Io(e))) => return Err(e),
                    Err(e) => panic!("failed to dispatch wayland events: {e}"),
                },
                Some(r) => {
                    self.state.read_guard = Some(r);
                    break Ok(());
                }
            }
        }
    }

    fn dispatch_events(&mut self) {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => {}
            Err(DispatchError::Backend(WaylandError::Io(e))) => {
                log::error!("Wayland Error: {e}");
            }
            Err(DispatchError::Backend(e)) => {
                panic!("backend error: {e}");
            }
            Err(DispatchError::BadMessage {
                sender_id,
                interface,
                opcode,
            }) => {
                panic!("bad message {sender_id}, {interface} , {opcode}");
            }
        }
    }

    fn flush_events(&mut self) -> io::Result<()> {
        // flush outgoing events
        match self.queue.flush() {
            Ok(_) => (),
            Err(e) => match e {
                WaylandError::Io(e) => {
                    return Err(e);
                }
                WaylandError::Protocol(e) => {
                    panic!("wayland protocol violation: {e}")
                }
            },
        }
        Ok(())
    }
}

#[async_trait]
impl Capture for LayerShellInputCapture {
    async fn create(
        &mut self,
        pos: Position,
        range: Option<EdgeRange>,
    ) -> Result<(), CaptureError> {
        self.add_client(pos, range);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.delete_client(pos);
        let inner = self.0.get_mut();
        Ok(inner.flush_events()?)
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        log::debug!("releasing pointer");
        let inner = self.0.get_mut();
        inner.state.ungrab();
        Ok(inner.flush_events()?)
    }

    /// a layer-shell surface grabs input only on pointer enter
    async fn focus(&mut self, _: Position, _: Option<EdgeRange>) -> Result<(), CaptureError> {
        Err(CaptureError::FocusByPointerPush)
    }

    /// local keys are invisible here: the compositor binds `cli focus`
    async fn set_hotkeys(&mut self, _: Vec<(crate::Chord, Position)>) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for LayerShellInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(event) = self.0.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(Ok(event)));
        }

        loop {
            let mut guard = ready!(self.0.poll_read_ready_mut(cx))?;

            {
                let inner = guard.get_inner_mut();

                // read events
                while inner.read() {
                    // prepare next read
                    match inner.prepare_read() {
                        Ok(_) => {}
                        Err(e) => return Poll::Ready(Some(Err(e.into()))),
                    }
                }

                // dispatch the events
                inner.dispatch_events();

                // flush outgoing events
                if let Err(e) = inner.flush_events() {
                    if e.kind() != ErrorKind::WouldBlock {
                        return Poll::Ready(Some(Err(e.into())));
                    }
                }

                // prepare for the next read
                match inner.prepare_read() {
                    Ok(_) => {}
                    Err(e) => return Poll::Ready(Some(Err(e.into()))),
                }
            }

            // clear read readiness for tokio read guard
            // guard.clear_ready_matching(Ready::READABLE);
            guard.clear_ready();

            // if an event has been queued during dispatch_events() we return it
            match guard.get_inner_mut().state.pending_events.pop_front() {
                Some(event) => return Poll::Ready(Some(Ok(event))),
                None => continue,
            }
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: <wl_seat::WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            if capabilities.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer.take() {
                    p.release();
                }
                state.pointer.replace(seat.get_pointer(qh, ()));
            }
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                state.keyboard.replace(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        app: &mut Self,
        pointer: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                // get client corresponding to the focused surface
                let Some(window) = app.active_windows.iter().find(|w| w.surface == surface) else {
                    return;
                };
                let window = window.clone();
                app.focused = Some(window.clone());
                app.grab(&surface, pointer, serial, qh);
                let point = (window.origin.0 + surface_x, window.origin.1 + surface_y);
                let at = geometry::edge_fraction(&app.displays(), window.pos, point);
                app.pending_events
                    .push_back((window.pos, CaptureEvent::Begin(at)));
            }
            wl_pointer::Event::Leave { .. } => {
                /* There are rare cases, where when a window is opened in
                 * just the wrong moment, the pointer is released, while
                 * still grabbed.
                 * In that case, the pointer must be ungrabbed, otherwise
                 * it is impossible to grab it again (since the pointer
                 * lock, relative pointer,... objects are still in place)
                 */
                if app.pointer_lock.is_some() {
                    log::warn!("compositor released mouse");
                    if let Some(window) = app.focused.as_ref() {
                        let pos = window.pos;
                        app.pending_events.push_back((pos, CaptureEvent::End));
                    }
                }
                app.ungrab();
            }
            wl_pointer::Event::Button {
                serial: _,
                time,
                button,
                state,
            } => {
                let Some(window) = app.focused.as_ref() else {
                    return;
                };
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                        time,
                        button,
                        state: u32::from(state),
                    })),
                ));
            }
            wl_pointer::Event::Axis { time, axis, value } => {
                let Some(window) = app.focused.as_ref() else {
                    return;
                };
                if app.scroll_discrete_pending {
                    // each axisvalue120 event is coupled with
                    // a corresponding axis event, which needs to
                    // be ignored to not duplicate the scrolling
                    app.scroll_discrete_pending = false;
                } else {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                            time,
                            axis: u32::from(axis) as u8,
                            value,
                        })),
                    ));
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let Some(window) = app.focused.as_ref() else {
                    return;
                };
                app.scroll_discrete_pending = true;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::AxisDiscrete120 {
                        axis: u32::from(axis) as u8,
                        value: value120,
                    })),
                ));
            }
            wl_pointer::Event::Frame => {
                // TODO properly handle frame events
                // we simply insert a frame event on the client side
                // after each event for now
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        app: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let window = &app.focused;
        match event {
            wl_keyboard::Event::Key {
                serial: _,
                time,
                key,
                state,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                            time,
                            key,
                            state: u32::from(state) as u8,
                        })),
                    ));
                }
            }
            wl_keyboard::Event::Modifiers {
                serial: _,
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
            } => {
                if let Some(window) = window {
                    app.pending_events.push_back((
                        window.pos,
                        CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Modifiers {
                            depressed: mods_depressed,
                            latched: mods_latched,
                            locked: mods_locked,
                            group,
                        })),
                    ));
                }
            }
            _ => (),
        }
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        app: &mut Self,
        _: &ZwpRelativePointerV1,
        event: <ZwpRelativePointerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            utime_hi,
            utime_lo,
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if let Some(window) = &app.focused {
                let time = ((((utime_hi as u64) << 32) | utime_lo as u64) / 1000) as u32;
                app.pending_events.push_back((
                    window.pos,
                    CaptureEvent::Input(Event::Pointer(PointerEvent::Motion { time, dx, dy })),
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        app: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = app
                .active_windows
                .iter()
                .find(|w| &w.layer_surface == layer_surface)
            {
                // client corresponding to the layer_surface
                let surface = &window.surface;
                let buffer = &window.buffer;
                surface.attach(Some(buffer), 0, 0);
                layer_surface.ack_configure(serial);
                surface.commit();
            }
        }
    }
}

// delegate wl_registry events to App itself
impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        state: &mut Self,
        _registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                state.register_global(Global {
                    name,
                    interface,
                    version,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.deregister_global(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: <ZxdgOutputV1 as wayland_client::Proxy>::Event,
        name: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let output = state
            .outputs
            .iter_mut()
            .find(|o| o.global.name == *name)
            .expect("output");

        log::debug!("xdg_output {name} - {event:?}");
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.pending_info.position = (x, y);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.pending_info.size = (width, height);
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Done => {
                log::warn!("Use of deprecated xdg-output event \"done\"");
                state.update_output_info(*name);
            }
            zxdg_output_v1::Event::Name { name } => {
                output.pending_info.name = name;
                output.has_xdg_info = true;
            }
            zxdg_output_v1::Event::Description { description } => {
                output.pending_info.description = description;
                output.has_xdg_info = true;
            }
            _ => todo!(),
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _wl_output: &WlOutput,
        event: <WlOutput as wayland_client::Proxy>::Event,
        name: &u32,
        _conn: &Connection,
        _qhandle: &QueueHandle<Self>,
    ) {
        log::debug!("wl_output {name} - {event:?}");
        if let wl_output::Event::Done = event {
            state.update_output_info(*name);
        }
    }
}

// don't emit any events
delegate_noop!(State: wl_region::WlRegion);
delegate_noop!(State: wl_shm_pool::WlShmPool);
delegate_noop!(State: wl_compositor::WlCompositor);
delegate_noop!(State: ZwlrLayerShellV1);
delegate_noop!(State: ZwpRelativePointerManagerV1);
delegate_noop!(State: ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ZwpPointerConstraintsV1);

// ignore events
delegate_noop!(State: ignore ZxdgOutputManagerV1);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use super::*;

    /// the globals the backend binds, and one output
    const GLOBALS: &[(&str, u32)] = &[
        ("wl_compositor", 5),
        ("wl_shm", 1),
        ("wl_seat", 7),
        ("zxdg_output_manager_v1", 3),
        ("zwlr_layer_shell_v1", 4),
        ("zwp_pointer_constraints_v1", 1),
        ("zwp_relative_pointer_manager_v1", 1),
        ("wl_output", 4),
    ];

    /// the output's logical size; it sits at (0, 0)
    const OUTPUT_SIZE: [u32; 2] = [1920, 1080];

    /// `zwlr_layer_surface_v1.anchor` bits
    const ANCHOR_BOTTOM: u32 = 2;
    const ANCHOR_RIGHT: u32 = 8;

    /// a Wayland message: object, opcode and the encoded arguments
    fn message(object: u32, opcode: u16, args: &[u32]) -> Vec<u8> {
        let size = (8 + 4 * args.len()) as u32;
        [object, size << 16 | u32::from(opcode)]
            .iter()
            .chain(args)
            .flat_map(|w| w.to_ne_bytes())
            .collect()
    }

    /// a `wl_registry.global` event
    fn global(registry: u32, name: u32, interface: &str, version: u32) -> Vec<u8> {
        let mut msg = Vec::new();
        // a string: length with the NUL, padded to 4 bytes
        msg.extend((interface.len() as u32 + 1).to_ne_bytes());
        msg.extend(interface.as_bytes());
        msg.resize(msg.len() + 4 - interface.len() % 4, 0);
        msg.extend(version.to_ne_bytes());
        let mut header = message(registry, 0, &[name]);
        let size = (header.len() + msg.len()) as u32;
        header[4..8].copy_from_slice(&(size << 16).to_ne_bytes());
        header.extend(msg);
        header
    }

    fn word(bytes: &[u8], at: usize) -> u32 {
        u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    /// a layer surface the client created and has not destroyed
    struct LayerSurface {
        surface: u32,
        anchor: u32,
        size: (u32, u32),
    }

    impl LayerSurface {
        /// the edge its barrier is on, once anchored and sized
        fn position(&self) -> Option<Position> {
            let right = self.anchor & ANCHOR_RIGHT != 0;
            let bottom = self.anchor & ANCHOR_BOTTOM != 0;
            match self.size {
                (1, h) if h > 1 => Some(if right {
                    Position::Right
                } else {
                    Position::Left
                }),
                (w, 1) if w > 1 => Some(if bottom {
                    Position::Bottom
                } else {
                    Position::Top
                }),
                _ => None,
            }
        }
    }

    /// what the compositor knows of the client's objects
    #[derive(Default)]
    struct Objects {
        /// the interface of each object whose requests matter here
        interfaces: HashMap<u32, String>,
        layer_surfaces: HashMap<u32, LayerSurface>,
    }

    impl Objects {
        /// handles a request, returning the events it answers with
        fn request(&mut self, object: u32, opcode: u32, args: &[u8]) -> Vec<u8> {
            let arg = |i: usize| word(args, 4 * i);
            let interface = match object {
                1 => "wl_display",
                _ => self.interfaces.get(&object).map_or("", String::as_str),
            };
            match (interface, opcode) {
                // sync: wl_callback.done
                ("wl_display", 0) => message(arg(0), 0, &[0]),
                ("wl_display", 1) => {
                    self.interfaces.insert(arg(0), "wl_registry".into());
                    (1..)
                        .zip(GLOBALS)
                        .flat_map(|(name, &(interface, version))| {
                            global(arg(0), name, interface, version)
                        })
                        .collect()
                }
                // bind: name, interface, version, new id
                ("wl_registry", 0) => {
                    let len = word(args, 4) as usize;
                    let interface = String::from_utf8(args[8..8 + len - 1].to_vec()).unwrap();
                    let id = word(args, 8 + len.next_multiple_of(4) + 4);
                    let reply = match interface.as_str() {
                        // capabilities: pointer and keyboard
                        "wl_seat" => message(id, 0, &[3]),
                        _ => vec![],
                    };
                    self.interfaces.insert(id, interface);
                    reply
                }
                ("wl_seat", 0) => {
                    self.interfaces.insert(arg(0), "wl_pointer".into());
                    vec![]
                }
                ("wl_seat", 1) => {
                    self.interfaces.insert(arg(0), "wl_keyboard".into());
                    vec![]
                }
                // get_xdg_output: logical position and size, then wl_output.done
                ("zxdg_output_manager_v1", 1) => {
                    let (xdg_output, output) = (arg(0), arg(1));
                    let mut reply = message(xdg_output, 0, &[0, 0]);
                    reply.extend(message(xdg_output, 1, &OUTPUT_SIZE));
                    reply.extend(message(output, 2, &[]));
                    reply
                }
                // get_layer_surface: new id, surface, ..
                ("zwlr_layer_shell_v1", 0) => {
                    self.interfaces
                        .insert(arg(0), "zwlr_layer_surface_v1".into());
                    let surface = LayerSurface {
                        surface: arg(1),
                        anchor: 0,
                        size: (0, 0),
                    };
                    self.layer_surfaces.insert(arg(0), surface);
                    vec![]
                }
                ("zwlr_layer_surface_v1", 0) => {
                    if let Some(s) = self.layer_surfaces.get_mut(&object) {
                        s.size = (arg(0), arg(1));
                    }
                    vec![]
                }
                ("zwlr_layer_surface_v1", 1) => {
                    if let Some(s) = self.layer_surfaces.get_mut(&object) {
                        s.anchor = arg(0);
                    }
                    vec![]
                }
                ("zwlr_layer_surface_v1", 7) => {
                    self.layer_surfaces.remove(&object);
                    vec![]
                }
                _ => vec![],
            }
        }

        /// the object implementing `interface`
        fn find(&self, interface: &str) -> u32 {
            let found = self.interfaces.iter().find(|(_, i)| *i == interface);
            *found.unwrap_or_else(|| panic!("no {interface}")).0
        }
    }

    /// The compositor end of a fake connection: it announces `GLOBALS`
    /// (one output, a seat with pointer and keyboard), answers
    /// `wl_display.sync` and tracks the barrier surfaces; [`Self::report`]
    /// sends the input a real compositor sends to them.
    pub(crate) struct Compositor {
        stream: Arc<Mutex<UnixStream>>,
        objects: Arc<Mutex<Objects>>,
        /// times the reader found nothing more to read
        drained: Arc<AtomicUsize>,
        serial: u32,
    }

    impl Compositor {
        fn serve(server: UnixStream) -> Self {
            server.set_nonblocking(true).unwrap();
            let mut reader = server.try_clone().unwrap();
            let compositor = Self {
                stream: Arc::new(Mutex::new(server)),
                objects: Arc::default(),
                drained: Arc::default(),
                serial: 0,
            };
            let (stream, objects, drained) = (
                compositor.stream.clone(),
                compositor.objects.clone(),
                compositor.drained.clone(),
            );
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0; 4096];
                // ends when the client hangs up
                loop {
                    match reader.read(&mut chunk) {
                        Ok(0) => return,
                        Ok(n) => buf.extend(&chunk[..n]),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => {
                            drained.fetch_add(1, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        }
                        Err(_) => return,
                    }
                    while buf.len() >= 8 && buf.len() >= (word(&buf, 4) >> 16) as usize {
                        let size = (word(&buf, 4) >> 16) as usize;
                        let request: Vec<u8> = buf.drain(..size).collect();
                        let (object, opcode) = (word(&request, 0), word(&request, 4) & 0xffff);
                        let reply = objects
                            .lock()
                            .unwrap()
                            .request(object, opcode, &request[8..]);
                        if stream.lock().unwrap().write_all(&reply).is_err() {
                            return;
                        }
                    }
                }
            });
            compositor
        }

        /// waits until every request sent so far has been handled
        fn settle(&self) {
            let start = self.drained.load(Ordering::SeqCst);
            let deadline = Instant::now() + Duration::from_secs(5);
            while self.drained.load(Ordering::SeqCst) < start + 2 {
                assert!(Instant::now() < deadline, "the fake compositor is stuck");
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        /// Makes the backend report `event` at `pos`, as a compositor does:
        /// `Begin` is the pointer entering the barrier at `pos`, `End` it
        /// leaving, keys and buttons come while it is there.
        pub(crate) fn report(&mut self, pos: Position, event: CaptureEvent) {
            self.settle();
            let objects = self.objects.lock().unwrap();
            // the newest: a replaced barrier's surfaces are destroyed first
            let surface = objects
                .layer_surfaces
                .iter()
                .filter(|(_, s)| s.position() == Some(pos))
                .max_by_key(|&(&id, _)| id)
                .unwrap_or_else(|| panic!("no barrier at {pos}"))
                .1
                .surface;
            let (pointer, keyboard) = (objects.find("wl_pointer"), objects.find("wl_keyboard"));
            drop(objects);
            self.serial += 1;
            let serial = self.serial;
            let msg = match event {
                CaptureEvent::Begin(_) => message(pointer, 0, &[serial, surface, 0, 0]),
                CaptureEvent::End => message(pointer, 1, &[serial, surface]),
                CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key { time, key, state })) => {
                    message(keyboard, 3, &[serial, time, key, state.into()])
                }
                CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                    time,
                    button,
                    state,
                })) => message(pointer, 3, &[serial, time, button, state]),
                other => panic!("the fake compositor cannot report {other:?}"),
            };
            self.stream.lock().unwrap().write_all(&msg).unwrap();
        }
    }

    /// a backend on a fake compositor that knows the output and the seat's
    /// devices before the first barrier
    pub(crate) fn on_fake_compositor() -> (LayerShellInputCapture, Compositor) {
        let (client, server) = UnixStream::pair().unwrap();
        let compositor = Compositor::serve(server);
        let conn = Connection::from_socket(client).unwrap();
        let mut capture = LayerShellInputCapture::bind(conn).unwrap();
        let inner = capture.0.get_mut();
        // a prepared read would block the roundtrips
        inner.state.read_guard = None;
        // the first dispatches the output and the seat's capabilities, the
        // second sends the pointer and keyboard requests they cause
        for _ in 0..2 {
            inner.queue.roundtrip(&mut inner.state).unwrap();
        }
        inner.prepare_read().unwrap();
        assert_eq!(inner.state.displays().len(), 1);
        (capture, compositor)
    }
}
