use futures::{StreamExt, future};
use std::{
    collections::HashSet,
    env, fs, io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::watch, task::JoinHandle};

use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use async_trait::async_trait;

use reis::{
    ei::{
        self, Button, Keyboard, Pointer, PointerAbsolute, Scroll, button::ButtonState,
        handshake::ContextType, keyboard::KeyState,
    },
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, SeatEvent},
    tokio::EiConvertEventStream,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::{PermissionPending, error::EmulationError};

use super::geometry::{Rect, entry_point};
use super::{Edge, Emulation, EmulationHandle, error::LibeiEmulationCreationError};

/// a device interface, with the device it belongs to
type Slot<T> = Arc<RwLock<Option<(ei::Device, T)>>>;

#[derive(Clone, Default)]
struct Devices {
    pointer: Slot<ei::Pointer>,
    /// absolute pointer with its regions (one per monitor on mutter)
    absolute: Slot<(ei::PointerAbsolute, Vec<Rect>)>,
    scroll: Slot<ei::Scroll>,
    button: Slot<ei::Button>,
    keyboard: Slot<ei::Keyboard>,
    /// devices the EIS resumed: events to a paused device are a protocol
    /// error
    resumed: Arc<RwLock<HashSet<ei::Device>>>,
}

impl Devices {
    /// the device and interface in `slot` if the device may emulate
    fn usable<'a, T>(&self, slot: &'a Option<(ei::Device, T)>) -> Option<&'a (ei::Device, T)> {
        let resumed = self.resumed.read().unwrap();
        slot.as_ref().filter(|(device, _)| resumed.contains(device))
    }

    /// forgets every interface of `device`
    fn remove(&self, device: &ei::Device) {
        fn clear<T>(slot: &Slot<T>, device: &ei::Device) {
            let mut slot = slot.write().unwrap();
            if slot.as_ref().is_some_and(|(d, _)| d == device) {
                slot.take();
            }
        }
        clear(&self.pointer, device);
        clear(&self.absolute, device);
        clear(&self.scroll, device);
        clear(&self.button, device);
        clear(&self.keyboard, device);
        self.resumed.write().unwrap().remove(device);
    }
}

pub(crate) struct LibeiEmulation {
    context: ei::Context,
    conn: event::Connection,
    devices: Devices,
    ei_task: JoinHandle<()>,
    error: Arc<Mutex<Option<EmulationError>>>,
    libei_error: Arc<AtomicBool>,
    _remote_desktop: RemoteDesktop,
    session: Session<RemoteDesktop>,
}

/// Get the path to the RemoteDesktop token file
fn get_token_file_path() -> PathBuf {
    let cache_dir = env::var("XDG_CACHE_HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var("HOME").expect("HOME not set");
            PathBuf::from(home).join(".cache")
        });

    cache_dir.join("mimouse").join("remote-desktop.token")
}

/// Read the RemoteDesktop token from file
fn read_token() -> Option<String> {
    let token_path = get_token_file_path();
    match fs::read_to_string(&token_path) {
        Ok(token) => Some(token.trim().to_string()),
        Err(_) => None,
    }
}

/// Write the RemoteDesktop token to file
fn write_token(token: &str) -> io::Result<()> {
    let token_path = get_token_file_path();
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&token_path, token)?;
    Ok(())
}

async fn get_ei_fd(
    permission_pending: &watch::Sender<bool>,
) -> Result<(RemoteDesktop, Session<RemoteDesktop>, OwnedFd), ashpd::Error> {
    let remote_desktop = RemoteDesktop::new().await?;

    let restore_token = read_token();

    log::debug!("creating session ...");
    let session = remote_desktop.create_session(Default::default()).await?;

    log::debug!("selecting devices ...");
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;

    log::info!("requesting permission for input emulation");
    let start_response = {
        let _pending = PermissionPending::new(permission_pending);
        remote_desktop
            .start(&session, None, Default::default())
            .await?
            .response()?
    };

    // The restore token is only valid once, we need to re-save it each time
    if let Some(token_str) = start_response.restore_token() {
        if let Err(e) = write_token(token_str) {
            log::warn!("failed to save RemoteDesktop token: {}", e);
        }
    }

    let fd = remote_desktop
        .connect_to_eis(&session, Default::default())
        .await?;
    Ok((remote_desktop, session, fd))
}

impl LibeiEmulation {
    pub(crate) async fn new(
        permission_pending: &watch::Sender<bool>,
    ) -> Result<Self, LibeiEmulationCreationError> {
        let (_remote_desktop, session, eifd) = get_ei_fd(permission_pending).await?;
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (conn, events) = context
            .handshake_tokio("io.github.cklaus1.MiMouse", ContextType::Sender)
            .await?;
        let devices = Devices::default();
        let libei_error = Arc::new(AtomicBool::default());
        let error = Arc::new(Mutex::new(None));
        let ei_handler = ei_task(
            events,
            conn.clone(),
            context.clone(),
            devices.clone(),
            libei_error.clone(),
            error.clone(),
        );
        let ei_task = tokio::task::spawn_local(ei_handler);

        Ok(Self {
            context,
            conn,
            devices,
            ei_task,
            error,
            libei_error,
            _remote_desktop,
            session,
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
    }
}

#[async_trait]
impl Emulation for LibeiEmulation {
    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;
        if self.libei_error.load(Ordering::SeqCst) {
            // don't break sending additional events but signal error
            if let Some(e) = self.error.lock().unwrap().take() {
                return Err(e);
            }
        }
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    let pointer_device = self.devices.pointer.read().unwrap();
                    if let Some((d, p)) = self.devices.usable(&pointer_device) {
                        p.motion_relative(dx as f32, dy as f32);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self.devices.button.read().unwrap();
                    if let Some((d, b)) = self.devices.usable(&button_device) {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = self.devices.usable(&scroll_device) {
                        match axis {
                            0 => s.scroll(0., value as f32),
                            _ => s.scroll(value as f32, 0.),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = self.devices.usable(&scroll_device) {
                        match axis {
                            0 => s.scroll_discrete(0, value),
                            _ => s.scroll_discrete(value, 0),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self.devices.keyboard.read().unwrap();
                    if let Some((d, k)) = self.devices.usable(&keyboard_device) {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    fn enter_at(
        &mut self,
        _: EmulationHandle,
        edge: Edge,
        fraction: u16,
    ) -> Result<Option<(i32, i32)>, EmulationError> {
        let point = {
            let absolute = self.devices.absolute.read().unwrap();
            let Some((d, (p, regions))) = self.devices.usable(&absolute) else {
                log::warn!("no usable absolute pointer device: pointer not placed");
                return Ok(None);
            };
            let Some((x, y)) = entry_point(regions, edge, fraction) else {
                log::warn!("absolute pointer device has no regions: pointer not placed");
                return Ok(None);
            };
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as u64;
            p.motion_absolute(x as f32, y as f32);
            d.frame(self.conn.serial(), now);
            (x, y)
        };
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(Some(point))
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}

    async fn terminate(&mut self) {
        let _ = self.session.close().await;
        self.ei_task.abort();
    }
}

async fn ei_task(
    mut events: EiConvertEventStream,
    _conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &context, &devices).await {
            Ok(()) => {}
            Err(e) => {
                libei_error.store(true, Ordering::SeqCst);
                error.lock().unwrap().replace(e);
                // wait for termination -> otherwise we will loop forever
                future::pending::<()>().await;
            }
        }
    }
}

async fn ei_event_handler(
    events: &mut EiConvertEventStream,
    context: &ei::Context,
    devices: &Devices,
) -> Result<(), EmulationError> {
    loop {
        let event = events.next().await.ok_or(EmulationError::EndOfStream)??;
        handle_ei_event(event, devices)?;
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
    }
}

/// tracks the devices the EIS offers and whether they may emulate
fn handle_ei_event(event: EiEvent, devices: &Devices) -> Result<(), EmulationError> {
    const CAPABILITIES: &[DeviceCapability] = &[
        DeviceCapability::Pointer,
        DeviceCapability::PointerAbsolute,
        DeviceCapability::Keyboard,
        DeviceCapability::Touch,
        DeviceCapability::Scroll,
        DeviceCapability::Button,
    ];
    log::debug!("{event:?}");
    match event {
        EiEvent::Disconnected(e) => {
            log::debug!("ei disconnected: {e:?}");
            return Err(EmulationError::EndOfStream);
        }
        EiEvent::SeatAdded(e) => {
            e.seat().bind_capabilities(CAPABILITIES);
        }
        EiEvent::SeatRemoved(e) => {
            log::debug!("seat removed: {:?}", e.seat());
        }
        EiEvent::DeviceAdded(e) => {
            let device_type = e.device().device_type();
            log::debug!("device added: {device_type:?}");
            let device = e.device();
            if let Some(pointer) = device.interface::<Pointer>() {
                devices
                    .pointer
                    .write()
                    .unwrap()
                    .replace((device.device().clone(), pointer));
            }
            if let Some(absolute) = device.interface::<PointerAbsolute>() {
                let regions = device
                    .regions()
                    .iter()
                    .map(|r| Rect {
                        x: r.x as i32,
                        y: r.y as i32,
                        width: r.width as i32,
                        height: r.height as i32,
                    })
                    .collect::<Vec<_>>();
                log::debug!("absolute pointer regions: {regions:?}");
                devices
                    .absolute
                    .write()
                    .unwrap()
                    .replace((device.device().clone(), (absolute, regions)));
            }
            if let Some(keyboard) = device.interface::<Keyboard>() {
                devices
                    .keyboard
                    .write()
                    .unwrap()
                    .replace((device.device().clone(), keyboard));
            }
            if let Some(scroll) = device.interface::<Scroll>() {
                devices
                    .scroll
                    .write()
                    .unwrap()
                    .replace((device.device().clone(), scroll));
            }
            if let Some(button) = device.interface::<Button>() {
                devices
                    .button
                    .write()
                    .unwrap()
                    .replace((device.device().clone(), button));
            }
        }
        EiEvent::DeviceRemoved(e) => {
            log::debug!("device removed: {:?}", e.device().device_type());
            devices.remove(e.device().device());
        }
        EiEvent::DevicePaused(e) => {
            log::debug!("device paused: {:?}", e.device().device_type());
            devices.resumed.write().unwrap().remove(e.device().device());
        }
        EiEvent::DeviceResumed(e) => {
            log::debug!("device resumed: {:?}", e.device().device_type());
            let device = e.device().device();
            device.start_emulating(0, 0);
            devices.resumed.write().unwrap().insert(device.clone());
        }
        EiEvent::KeyboardModifiers(e) => {
            log::debug!("modifiers: {e:?}");
        }
        // only for receiver context
        // EiEvent::Frame(_) => { },
        // EiEvent::DeviceStartEmulating(_) => { },
        // EiEvent::DeviceStopEmulating(_) => { },
        // EiEvent::PointerMotion(_) => { },
        // EiEvent::PointerMotionAbsolute(_) => { },
        // EiEvent::Button(_) => { },
        // EiEvent::ScrollDelta(_) => { },
        // EiEvent::ScrollStop(_) => { },
        // EiEvent::ScrollCancel(_) => { },
        // EiEvent::ScrollDiscrete(_) => { },
        // EiEvent::KeyboardKey(_) => { },
        // EiEvent::TouchDown(_) => { },
        // EiEvent::TouchUp(_) => { },
        // EiEvent::TouchMotion(_) => { },
        _ => unreachable!("unexpected ei event"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use reis::{
        PendingRequestResult, eis,
        handshake::EisHandshaker,
        request::{EisRequest, EisRequestConverter},
    };
    use tokio::io::unix::AsyncFd;

    /// requests the client sent, waiting for at least one
    async fn requests(eis: &AsyncFd<eis::Context>) -> Vec<eis::Request> {
        loop {
            let mut ready = eis.readable().await.unwrap();
            match eis.get_ref().read() {
                Ok(0) => panic!("the client hung up"),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    ready.clear_ready();
                    continue;
                }
                Err(e) => panic!("{e}"),
            }
            let mut requests = Vec::new();
            while let Some(request) = eis.get_ref().pending_request() {
                let PendingRequestResult::Request(request) = request else {
                    panic!("malformed request");
                };
                requests.push(request);
            }
            if !requests.is_empty() {
                return requests;
            }
        }
    }

    async fn eis_handshake(eis: &AsyncFd<eis::Context>) -> EisRequestConverter {
        let mut handshaker = EisHandshaker::new(eis.get_ref(), 1);
        loop {
            for request in requests(eis).await {
                if let Some(done) = handshaker.handle_request(request).unwrap() {
                    let converter = EisRequestConverter::new(eis.get_ref(), done, 1);
                    // the client waits for the connection object
                    eis.get_ref().flush().unwrap();
                    return converter;
                }
            }
        }
    }

    /// handles events until one `stop` holds
    async fn handle_until(
        events: &mut EiConvertEventStream,
        context: &ei::Context,
        devices: &Devices,
        stop: fn(&EiEvent) -> bool,
    ) {
        loop {
            let event = events.next().await.unwrap().unwrap();
            let last = stop(&event);
            handle_ei_event(event, devices).unwrap();
            context.flush().unwrap();
            if last {
                return;
            }
        }
    }

    #[tokio::test]
    async fn only_resumed_devices_emulate() {
        let (client, server) = UnixStream::pair().unwrap();
        let eis = AsyncFd::new(eis::Context::new(server).unwrap()).unwrap();
        let context = ei::Context::new(client).unwrap();
        let ((_conn, mut events), mut eis_conn) = tokio::join!(
            async {
                context
                    .handshake_tokio("test", ContextType::Sender)
                    .await
                    .unwrap()
            },
            eis_handshake(&eis),
        );
        let devices = Devices::default();
        let keyboard_usable = || devices.usable(&devices.keyboard.read().unwrap()).is_some();

        let seat = eis_conn
            .handle()
            .add_seat(Some("seat"), &[DeviceCapability::Keyboard]);
        eis.get_ref().flush().unwrap();
        handle_until(&mut events, &context, &devices, |e| {
            matches!(e, EiEvent::SeatAdded(_))
        })
        .await;
        'bind: loop {
            for request in requests(&eis).await {
                eis_conn.handle_request(request).unwrap();
            }
            while let Some(request) = eis_conn.next_request() {
                if matches!(request, EisRequest::Bind(_)) {
                    break 'bind;
                }
            }
        }

        let device = seat.add_device(
            Some("keyboard"),
            eis::device::DeviceType::Virtual,
            &[DeviceCapability::Keyboard],
            |_| {},
        );
        eis.get_ref().flush().unwrap();
        handle_until(&mut events, &context, &devices, |e| {
            matches!(e, EiEvent::DeviceAdded(_))
        })
        .await;
        // devices start paused
        assert!(devices.keyboard.read().unwrap().is_some());
        assert!(!keyboard_usable());

        device.resumed();
        eis.get_ref().flush().unwrap();
        handle_until(&mut events, &context, &devices, |e| {
            matches!(e, EiEvent::DeviceResumed(_))
        })
        .await;
        assert!(keyboard_usable());

        device.paused();
        eis.get_ref().flush().unwrap();
        handle_until(&mut events, &context, &devices, |e| {
            matches!(e, EiEvent::DevicePaused(_))
        })
        .await;
        assert!(!keyboard_usable());

        device.resumed();
        device.remove();
        eis.get_ref().flush().unwrap();
        handle_until(&mut events, &context, &devices, |e| {
            matches!(e, EiEvent::DeviceRemoved(_))
        })
        .await;
        assert!(devices.keyboard.read().unwrap().is_none());
        assert!(devices.resumed.read().unwrap().is_empty());
    }
}
