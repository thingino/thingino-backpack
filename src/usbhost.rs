//! A `tdfu-usb` backend on the ESP-IDF USB Host Library.
//!
//! The library has no transfer timeouts (`usb_transfer_t::timeout_ms` is documented as
//! unsupported), so every deadline here is ours. A transfer we stop waiting for is
//! abandoned to its completion callback, which frees it whenever it finally completes.
//! Only endpoints of a claimed interface can be cancelled (halt + flush): an EP0 transfer
//! the device never answers stays in flight until the device leaves the bus, which is why
//! `reset` power-cycles the root port.

use core::ffi::{c_void, CStr};
use core::future::poll_fn;
use core::ptr;
use core::task::Poll;
use core::time::Duration;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::Instant;

use esp_idf_svc::sys;
use log::{info, warn};
use tdfu_usb::{
    BulkEndpoint, ControlIn, ControlOut, ControlType, DeviceDescriptors, Direction, Discovered, InterfaceSpec,
    LocalUsbBackend, LocalUsbTransport, Pipe, Recipient, UsbError, UsbErrorKind,
};

const SETUP_LEN: usize = 8;
/// The largest single transfer buffer. It is DMA memory, which the S3 has little of, and a
/// multiple of every bulk max packet size so that splitting never inserts a short packet.
const CHUNK: usize = 16 * 1024;
const DEVICE_GONE_TIMEOUT: Duration = Duration::from_secs(2);
const REENUMERATE_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

const OK: sys::esp_err_t = sys::ESP_OK as sys::esp_err_t;
const ERR_NOT_FOUND: sys::esp_err_t = sys::ESP_ERR_NOT_FOUND as sys::esp_err_t;
const ERR_INVALID_STATE: sys::esp_err_t = sys::ESP_ERR_INVALID_STATE as sys::esp_err_t;

type DevHandle = sys::usb_device_handle_t;

/// A library handle: an opaque token that only the library dereferences.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Raw<T>(T);
unsafe impl<T> Send for Raw<T> {}
unsafe impl<T> Sync for Raw<T> {}

struct Shared {
    client: OnceLock<Raw<sys::usb_host_client_handle_t>>,
    state: Mutex<HostState>,
    changed: Condvar,
}

#[derive(Default)]
struct HostState {
    /// Handles the library reported gone that are not closed yet.
    gone: Vec<usize>,
    /// Devices whose close waits for abandoned EP0 transfers: IDF asserts when a device
    /// is closed with a control transfer in flight, and such a transfer only completes
    /// once the device leaves the bus.
    deferred: Vec<Deferred>,
    /// The library refuses a second open from the same client, so `list` answers for the
    /// devices open right now from here.
    open: HashMap<u8, DeviceDescriptors>,
}

impl Shared {
    fn client(&self) -> sys::usb_host_client_handle_t {
        self.client.get().expect("client registered at install").0
    }

    fn wait_until(&self, timeout: Duration, mut done: impl FnMut(&HostState) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().unwrap();
        loop {
            if done(&state) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            state = self.changed.wait_timeout(state, deadline - now).unwrap().0;
        }
    }

    fn forget(&self, dev: DevHandle, address: u8) {
        let mut state = self.state.lock().unwrap();
        state.gone.retain(|&handle| handle != dev as usize);
        state.open.remove(&address);
    }

    /// Closes deferred devices whose abandoned transfers have all completed.
    fn reap(&self) {
        let mut state = self.state.lock().unwrap();
        let HostState { gone, deferred, open } = &mut *state;
        deferred.retain(|entry| {
            if entry.ep0_inflight.load(Ordering::SeqCst) > 0 {
                return true;
            }
            unsafe { sys::usb_host_device_close(self.client(), entry.handle as DevHandle) };
            gone.retain(|&handle| handle != entry.handle);
            open.remove(&entry.address);
            false
        });
    }
}

struct Deferred {
    handle: usize,
    address: u8,
    ep0_inflight: Arc<AtomicUsize>,
}

unsafe extern "C" fn client_event(msg: *const sys::usb_host_client_event_msg_t, arg: *mut c_void) {
    let shared = &*(arg as *const Shared);
    let msg = &*msg;
    let mut state = shared.state.lock().unwrap();
    match msg.event {
        sys::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_NEW_DEV => {
            info!("usb: new device at address {}", msg.__bindgen_anon_1.new_dev.address);
        }
        sys::usb_host_client_event_t_USB_HOST_CLIENT_EVENT_DEV_GONE => {
            info!("usb: an open device is gone");
            state.gone.push(msg.__bindgen_anon_1.dev_gone.dev_hdl as usize);
        }
        _ => {}
    }
    shared.changed.notify_all();
}

/// The installed library and this program's one client.
pub struct UsbHost {
    shared: Arc<Shared>,
}

impl UsbHost {
    /// Installs the library on the internal PHY. Call once.
    pub fn install() -> Result<Self, UsbError> {
        let mut config: sys::usb_host_config_t = unsafe { core::mem::zeroed() };
        config.intr_flags = sys::ESP_INTR_FLAG_LEVEL1 as i32;
        check(unsafe { sys::usb_host_install(&config) }, Pipe::Device, "usb_host_install")?;
        spawn("usb-lib", || loop {
            let mut flags = 0;
            unsafe { sys::usb_host_lib_handle_events(u32::MAX, &mut flags) };
        });

        let shared = Arc::new(Shared {
            client: OnceLock::new(),
            state: Mutex::default(),
            changed: Condvar::new(),
        });
        let mut client_config: sys::usb_host_client_config_t = unsafe { core::mem::zeroed() };
        client_config.max_num_event_msg = 8;
        client_config.__bindgen_anon_1.async_.client_event_callback = Some(client_event);
        // The client is never deregistered, so this reference is never released.
        client_config.__bindgen_anon_1.async_.callback_arg = Arc::into_raw(shared.clone()) as *mut c_void;
        let mut client = ptr::null_mut();
        check(
            unsafe { sys::usb_host_client_register(&client_config, &mut client) },
            Pipe::Device,
            "usb_host_client_register",
        )?;
        let client = Raw(client);
        let _ = shared.client.set(client);
        spawn("usb-client", move || {
            let client = client;
            loop {
                unsafe { sys::usb_host_client_handle_events(client.0, u32::MAX) };
            }
        });
        Ok(Self { shared })
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) {
    thread::Builder::new()
        .name(name.into())
        .stack_size(4096)
        .spawn(body)
        .expect("spawning a USB task");
}

fn check(err: sys::esp_err_t, pipe: Pipe, what: &str) -> Result<(), UsbError> {
    match err {
        OK => Ok(()),
        ERR_NOT_FOUND => Err(UsbError::new(UsbErrorKind::NoDevice, pipe)),
        _ => Err(UsbError::new(UsbErrorKind::Backend(format!("{what}: {}", err_name(err))), pipe)),
    }
}

fn err_name(err: sys::esp_err_t) -> String {
    unsafe { CStr::from_ptr(sys::esp_err_to_name(err)) }.to_string_lossy().into_owned()
}

fn status_error(status: sys::usb_transfer_status_t, pipe: Pipe) -> Result<(), UsbError> {
    let kind = match status {
        sys::usb_transfer_status_t_USB_TRANSFER_STATUS_COMPLETED => return Ok(()),
        sys::usb_transfer_status_t_USB_TRANSFER_STATUS_STALL => UsbErrorKind::Stall,
        sys::usb_transfer_status_t_USB_TRANSFER_STATUS_NO_DEVICE => UsbErrorKind::NoDevice,
        sys::usb_transfer_status_t_USB_TRANSFER_STATUS_OVERFLOW => UsbErrorKind::Overflow,
        sys::usb_transfer_status_t_USB_TRANSFER_STATUS_TIMED_OUT => UsbErrorKind::Timeout,
        _ => UsbErrorKind::Fault,
    };
    Err(UsbError::new(kind, pipe))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Waiting,
    Done,
    Abandoned,
}

struct Completion {
    slot: Mutex<Slot>,
    done: Condvar,
    /// The device's count of abandoned EP0 transfers, for control transfers only.
    ep0_inflight: Option<Arc<AtomicUsize>>,
}

/// Runs on the client task. An in-flight transfer owns one reference to its completion.
unsafe extern "C" fn transfer_done(transfer: *mut sys::usb_transfer_t) {
    let completion = Arc::from_raw((*transfer).context as *const Completion);
    let mut slot = completion.slot.lock().unwrap();
    if *slot == Slot::Abandoned {
        sys::usb_host_transfer_free(transfer);
        if let Some(count) = &completion.ep0_inflight {
            count.fetch_sub(1, Ordering::SeqCst);
        }
    } else {
        *slot = Slot::Done;
        completion.done.notify_all();
    }
}

struct Transfer {
    raw: *mut sys::usb_transfer_t,
    completion: Arc<Completion>,
    /// Cleared when the transfer is abandoned; its callback frees it from then on.
    owned: bool,
}

impl Transfer {
    fn alloc(len: usize, pipe: Pipe, ep0_inflight: Option<Arc<AtomicUsize>>) -> Result<Self, UsbError> {
        let mut raw = ptr::null_mut();
        check(unsafe { sys::usb_host_transfer_alloc(len, 0, &mut raw) }, pipe, "usb_host_transfer_alloc")?;
        Ok(Self {
            raw,
            completion: Arc::new(Completion {
                slot: Mutex::new(Slot::Waiting),
                done: Condvar::new(),
                ep0_inflight,
            }),
            owned: true,
        })
    }

    fn buffer(&mut self) -> &mut [u8] {
        unsafe { core::slice::from_raw_parts_mut((*self.raw).data_buffer, (*self.raw).data_buffer_size) }
    }

    fn prepare(&mut self, dev: DevHandle, endpoint: u8, num_bytes: usize) {
        unsafe {
            (*self.raw).device_handle = dev;
            (*self.raw).bEndpointAddress = endpoint;
            (*self.raw).num_bytes = num_bytes as i32;
            (*self.raw).callback = Some(transfer_done);
        }
    }

    /// `Ok` once the transfer completed, whatever its status; `Timeout` once abandoned.
    fn submit_and_wait(
        &mut self,
        submit: impl FnOnce(*mut sys::usb_transfer_t) -> sys::esp_err_t,
        timeout: Duration,
        pipe: Pipe,
    ) -> Result<(), UsbError> {
        let context = Arc::into_raw(self.completion.clone());
        unsafe { (*self.raw).context = context as *mut c_void };
        let err = submit(self.raw);
        if err != OK {
            drop(unsafe { Arc::from_raw(context) });
            return check(err, pipe, "transfer submit");
        }
        let deadline = Instant::now() + timeout;
        let mut slot = self.completion.slot.lock().unwrap();
        while *slot != Slot::Done {
            let now = Instant::now();
            if now >= deadline {
                *slot = Slot::Abandoned;
                if let Some(count) = &self.completion.ep0_inflight {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                self.owned = false;
                return Err(UsbError::new(UsbErrorKind::Timeout, pipe).with_timeout(timeout));
            }
            slot = self.completion.done.wait_timeout(slot, deadline - now).unwrap().0;
        }
        Ok(())
    }

    fn status(&self) -> sys::usb_transfer_status_t {
        unsafe { (*self.raw).status }
    }

    fn actual(&self) -> usize {
        usize::try_from(unsafe { (*self.raw).actual_num_bytes }).unwrap_or(0)
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if self.owned {
            unsafe { sys::usb_host_transfer_free(self.raw) };
        }
    }
}

struct Described {
    descriptors: DeviceDescriptors,
    mps0: usize,
    configuration: u8,
}

fn describe(dev: DevHandle, address: u8) -> Result<Described, UsbError> {
    let mut info: sys::usb_device_info_t = unsafe { core::mem::zeroed() };
    check(unsafe { sys::usb_host_device_info(dev, &mut info) }, Pipe::Device, "usb_host_device_info")?;
    let mut device: *const sys::usb_device_desc_t = ptr::null();
    check(
        unsafe { sys::usb_host_get_device_descriptor(dev, &mut device) },
        Pipe::Device,
        "usb_host_get_device_descriptor",
    )?;
    let device = unsafe { (*device).val };
    let mut config: *const sys::usb_config_desc_t = ptr::null();
    check(
        unsafe { sys::usb_host_get_active_config_descriptor(dev, &mut config) },
        Pipe::Device,
        "usb_host_get_active_config_descriptor",
    )?;
    let head = unsafe { (*config).val };
    let total = usize::from(u16::from_le_bytes([head[2], head[3]]));
    let config = unsafe { core::slice::from_raw_parts(config.cast::<u8>(), total) }.to_vec();

    let mut descriptors = DeviceDescriptors::new(
        u16::from_le_bytes([device[8], device[9]]),
        u16::from_le_bytes([device[10], device[11]]),
    )
    .with_bus_address(1, address)
    // One root port and no hub support: every device is on port 1, before and after it
    // re-enumerates, which is the identity thingino-dfu tracks across a bootstrap.
    .with_port_path(vec![1])
    .with_config_descriptor(config);
    if let Some(product) = string_descriptor(info.str_desc_product) {
        descriptors = descriptors.with_product_string(product);
    }
    Ok(Described {
        descriptors,
        mps0: usize::from(info.bMaxPacketSize0),
        configuration: info.bConfigurationValue,
    })
}

fn string_descriptor(desc: *const sys::usb_str_desc_t) -> Option<String> {
    if desc.is_null() {
        return None;
    }
    let bytes = desc.cast::<u8>();
    let len = usize::from(unsafe { *bytes });
    let body = unsafe { core::slice::from_raw_parts(bytes.add(2), len.checked_sub(2)?) };
    let units: Vec<u16> = body.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    Some(String::from_utf16_lossy(&units))
}

/// `wMaxPacketSize` of `address` in alternate setting 0 of `interface`.
fn endpoint_mps(config: &[u8], interface: u8, address: u8) -> Option<usize> {
    let mut current = None;
    let mut rest = config;
    while rest.len() >= 2 {
        let len = usize::from(rest[0]);
        if len < 2 || len > rest.len() {
            break;
        }
        let desc = &rest[..len];
        match desc[1] {
            0x04 if len >= 4 => current = Some((desc[2], desc[3])),
            0x05 if len >= 7 && current == Some((interface, 0)) && desc[2] == address => {
                return Some(usize::from(u16::from_le_bytes([desc[4], desc[5]]) & 0x7ff));
            }
            _ => {}
        }
        rest = &rest[len..];
    }
    None
}

/// Every endpoint address in alternate setting 0 of `interface`.
fn interface_endpoints(config: &[u8], interface: u8) -> Vec<u8> {
    let mut current = None;
    let mut found = Vec::new();
    let mut rest = config;
    while rest.len() >= 2 {
        let len = usize::from(rest[0]);
        if len < 2 || len > rest.len() {
            break;
        }
        let desc = &rest[..len];
        match desc[1] {
            0x04 if len >= 4 => current = Some((desc[2], desc[3])),
            0x05 if len >= 3 && current == Some((interface, 0)) => found.push(desc[2]),
            _ => {}
        }
        rest = &rest[len..];
    }
    found
}

fn request_type(direction: Direction, control_type: ControlType, recipient: Recipient) -> u8 {
    let direction = match direction {
        Direction::In => 0x80,
        Direction::Out => 0x00,
    };
    let control_type = match control_type {
        ControlType::Standard => 0 << 5,
        ControlType::Class => 1 << 5,
        ControlType::Vendor => 2 << 5,
    };
    let recipient = match recipient {
        Recipient::Device => 0,
        Recipient::Interface => 1,
        Recipient::Endpoint => 2,
        Recipient::Other => 3,
    };
    direction | control_type | recipient
}

impl LocalUsbBackend for UsbHost {
    type Transport = EspTransport;
    type DeviceId = u8;

    async fn list(&self) -> Result<Vec<Discovered<u8>>, UsbError> {
        self.shared.reap();
        let mut addresses = [0u8; 16];
        let mut count = 0;
        check(
            unsafe { sys::usb_host_device_addr_list_fill(addresses.len() as i32, addresses.as_mut_ptr(), &mut count) },
            Pipe::Device,
            "usb_host_device_addr_list_fill",
        )?;
        let client = self.shared.client();
        let mut found = Vec::new();
        for &address in &addresses[..usize::try_from(count).unwrap_or(0)] {
            let cached = self.shared.state.lock().unwrap().open.get(&address).cloned();
            if let Some(descriptors) = cached {
                found.push(Discovered { id: address, descriptors });
                continue;
            }
            let mut dev = ptr::null_mut();
            // A device that left between the fill and the open is not a listing error.
            if unsafe { sys::usb_host_device_open(client, address, &mut dev) } != OK {
                continue;
            }
            let described = describe(dev, address);
            unsafe { sys::usb_host_device_close(client, dev) };
            if let Ok(described) = described {
                found.push(Discovered {
                    id: address,
                    descriptors: described.descriptors,
                });
            }
        }
        Ok(found)
    }

    async fn open(&self, id: &u8) -> Result<EspTransport, UsbError> {
        let client = self.shared.client();
        let mut dev = ptr::null_mut();
        check(unsafe { sys::usb_host_device_open(client, *id, &mut dev) }, Pipe::Device, "usb_host_device_open")?;
        let described = match describe(dev, *id) {
            Ok(described) => described,
            Err(err) => {
                unsafe { sys::usb_host_device_close(client, dev) };
                return Err(err);
            }
        };
        self.shared.state.lock().unwrap().open.insert(*id, described.descriptors.clone());
        Ok(EspTransport {
            shared: self.shared.clone(),
            dev: Cell::new(Raw(dev)),
            address: Cell::new(*id),
            descriptors: described.descriptors,
            mps0: described.mps0,
            configuration: described.configuration,
            claim: RefCell::new(None),
            idf_claimed: Cell::new(None),
            ep0_inflight: RefCell::new(Arc::new(AtomicUsize::new(0))),
        })
    }
}

#[derive(Clone, Copy)]
struct Claim {
    interface: u8,
    bulk_in: Option<(BulkEndpoint, usize)>,
    bulk_out: Option<(BulkEndpoint, usize)>,
}

impl Claim {
    fn endpoints(self) -> impl Iterator<Item = BulkEndpoint> {
        [self.bulk_in, self.bulk_out].into_iter().flatten().map(|(endpoint, _)| endpoint)
    }
}

/// One open device.
pub struct EspTransport {
    shared: Arc<Shared>,
    dev: Cell<Raw<DevHandle>>,
    address: Cell<u8>,
    descriptors: DeviceDescriptors,
    mps0: usize,
    configuration: u8,
    claim: RefCell<Option<Claim>>,
    /// The interface claimed from IDF, which outlives the logical `claim`: see
    /// `claim_interface`.
    idf_claimed: Cell<Option<u8>>,
    /// Replaced on `reset`, so a deferred old handle keeps its own count.
    ep0_inflight: RefCell<Arc<AtomicUsize>>,
}

impl EspTransport {
    fn dev(&self) -> DevHandle {
        self.dev.get().0
    }

    fn is_gone(&self) -> bool {
        self.shared.state.lock().unwrap().gone.contains(&(self.dev() as usize))
    }

    #[allow(clippy::too_many_arguments)]
    fn control(
        &self,
        direction: Direction,
        control_type: ControlType,
        recipient: Recipient,
        request: u8,
        value: u16,
        index: u16,
        out: &[u8],
        in_len: u16,
        timeout: Duration,
    ) -> Result<Vec<u8>, UsbError> {
        let pipe = Pipe::Control { direction, request };
        if self.is_gone() {
            return Err(UsbError::new(UsbErrorKind::NoDevice, pipe));
        }
        // IDF wants an IN data stage rounded up to the packet size, and an OUT one exact.
        let (w_length, data_len) = match direction {
            Direction::In => (in_len, usize::from(in_len).div_ceil(self.mps0) * self.mps0),
            Direction::Out => (
                u16::try_from(out.len()).map_err(|_| UsbError::new(UsbErrorKind::Fault, pipe).with_len(out.len()))?,
                out.len(),
            ),
        };
        let mut transfer = Transfer::alloc(SETUP_LEN + data_len, pipe, Some(self.ep0_inflight.borrow().clone()))?;
        let buffer = transfer.buffer();
        buffer[0] = request_type(direction, control_type, recipient);
        buffer[1] = request;
        buffer[2..4].copy_from_slice(&value.to_le_bytes());
        buffer[4..6].copy_from_slice(&index.to_le_bytes());
        buffer[6..8].copy_from_slice(&w_length.to_le_bytes());
        if direction == Direction::Out {
            buffer[SETUP_LEN..SETUP_LEN + out.len()].copy_from_slice(out);
        }
        transfer.prepare(self.dev(), 0, SETUP_LEN + data_len);
        let client = self.shared.client();
        transfer.submit_and_wait(|raw| unsafe { sys::usb_host_transfer_submit_control(client, raw) }, timeout, pipe)?;
        status_error(transfer.status(), pipe)?;
        // `actual_num_bytes` counts the setup packet.
        let got = transfer.actual().saturating_sub(SETUP_LEN);
        match direction {
            Direction::In => {
                let got = got.min(usize::from(in_len));
                Ok(transfer.buffer()[SETUP_LEN..SETUP_LEN + got].to_vec())
            }
            Direction::Out if got < out.len() => {
                Err(UsbError::new(UsbErrorKind::Short { got, want: out.len() }, pipe))
            }
            Direction::Out => Ok(Vec::new()),
        }
    }

    /// Host side only: halting and flushing completes whatever is queued as cancelled.
    fn cancel(&self, endpoint: BulkEndpoint) {
        self.cancel_address(endpoint.address());
    }

    fn cancel_address(&self, address: u8) {
        let dev = self.dev();
        unsafe {
            sys::usb_host_endpoint_halt(dev, address);
            sys::usb_host_endpoint_flush(dev, address);
            sys::usb_host_endpoint_clear(dev, address);
        }
    }

    fn claim_idf(&self, interface: u8) -> Result<(), UsbError> {
        let err = unsafe { sys::usb_host_interface_claim(self.shared.client(), self.dev(), interface, 0) };
        if err == ERR_INVALID_STATE {
            return Err(UsbError::new(UsbErrorKind::Busy, Pipe::Device));
        }
        check(err, Pipe::Device, "usb_host_interface_claim")?;
        self.idf_claimed.set(Some(interface));
        Ok(())
    }

    fn release_idf(&self) {
        let Some(interface) = self.idf_claimed.take() else {
            return;
        };
        let client = self.shared.client();
        if unsafe { sys::usb_host_interface_release(client, self.dev(), interface) } == ERR_INVALID_STATE {
            // Abandoned transfers still queued on the interface's endpoints.
            for address in interface_endpoints(&self.descriptors.config_descriptor, interface) {
                self.cancel_address(address);
            }
            unsafe { sys::usb_host_interface_release(client, self.dev(), interface) };
        }
    }

    fn claimed(&self) -> Option<Claim> {
        *self.claim.borrow()
    }

    fn not_claimed(pipe: Pipe) -> UsbError {
        UsbError::new(UsbErrorKind::NotClaimed, pipe)
    }

    /// Best effort, for teardown paths that have no one to report to.
    fn release_claim(&self) {
        self.claim.borrow_mut().take();
        self.release_idf();
    }

    fn close(&self) {
        let dev = self.dev();
        self.release_claim();
        let ep0_inflight = self.ep0_inflight.borrow().clone();
        if ep0_inflight.load(Ordering::SeqCst) > 0 {
            warn!("address {}: a control transfer is still in flight; closing once it completes", self.address.get());
            self.shared.state.lock().unwrap().deferred.push(Deferred {
                handle: dev as usize,
                address: self.address.get(),
                ep0_inflight,
            });
            return;
        }
        let err = unsafe { sys::usb_host_device_close(self.shared.client(), dev) };
        self.shared.forget(dev, self.address.get());
        if err != OK {
            warn!("closing address {}: {}", self.address.get(), err_name(err));
        }
    }
}

impl Drop for EspTransport {
    fn drop(&mut self) {
        self.close();
    }
}

/// Let the executor run once. A transfer here blocks the calling thread until the device
/// answers, so without this an operation's whole transfer loop would run inside a single
/// poll, and whatever drives it (the daemon's progress pump) could send nothing until the
/// loop ended, while every progress event piled up in its queue.
async fn yield_now() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}

impl LocalUsbTransport for EspTransport {
    async fn control_in(&self, req: ControlIn, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let answer = self.control(
            Direction::In,
            req.control_type,
            req.recipient,
            req.request,
            req.value,
            req.index,
            &[],
            req.len,
            timeout,
        );
        yield_now().await;
        answer
    }

    async fn control_out(&self, req: ControlOut<'_>, timeout: Duration) -> Result<(), UsbError> {
        let answer = self.control(
            Direction::Out,
            req.control_type,
            req.recipient,
            req.request,
            req.value,
            req.index,
            req.data,
            0,
            timeout,
        );
        yield_now().await;
        answer.map(drop)
    }

    async fn bulk_out(&self, data: &[u8], timeout: Duration) -> Result<usize, UsbError> {
        let Some((endpoint, _)) = self.claimed().and_then(|claim| claim.bulk_out) else {
            return Err(Self::not_claimed(Pipe::Device));
        };
        let pipe = Pipe::Bulk(endpoint);
        let mut sent = 0;
        for chunk in data.chunks(CHUNK) {
            yield_now().await;
            let mut transfer = Transfer::alloc(chunk.len(), pipe, None)?;
            transfer.buffer()[..chunk.len()].copy_from_slice(chunk);
            transfer.prepare(self.dev(), endpoint.address(), chunk.len());
            if let Err(err) =
                transfer.submit_and_wait(|raw| unsafe { sys::usb_host_transfer_submit(raw) }, timeout, pipe)
            {
                self.cancel(endpoint);
                // Progress already made is reported as such, so the retry resumes after it.
                return Err(if sent > 0 {
                    UsbError::new(UsbErrorKind::Short { got: sent, want: data.len() }, pipe)
                } else {
                    err
                });
            }
            status_error(transfer.status(), pipe).map_err(|err| err.with_transferred(sent))?;
            sent += transfer.actual();
            if transfer.actual() < chunk.len() {
                return Err(UsbError::new(UsbErrorKind::Short { got: sent, want: data.len() }, pipe));
            }
        }
        Ok(sent)
    }

    async fn bulk_in(&self, len: usize, timeout: Duration) -> Result<Vec<u8>, UsbError> {
        let Some((endpoint, mps)) = self.claimed().and_then(|claim| claim.bulk_in) else {
            return Err(Self::not_claimed(Pipe::Device));
        };
        let pipe = Pipe::Bulk(endpoint);
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            yield_now().await;
            let want = (len - out.len()).min(CHUNK);
            let request = want.div_ceil(mps) * mps;
            let mut transfer = Transfer::alloc(request, pipe, None)?;
            transfer.prepare(self.dev(), endpoint.address(), request);
            if let Err(err) =
                transfer.submit_and_wait(|raw| unsafe { sys::usb_host_transfer_submit(raw) }, timeout, pipe)
            {
                self.cancel(endpoint);
                return Err(err);
            }
            status_error(transfer.status(), pipe)?;
            let got = transfer.actual().min(want);
            out.extend_from_slice(&transfer.buffer()[..got]);
            if got < want {
                break;
            }
        }
        if out.len() < len {
            return Err(UsbError::new(UsbErrorKind::Short { got: out.len(), want: len }, pipe));
        }
        Ok(out)
    }

    async fn set_configuration(&self, value: u8) -> Result<(), UsbError> {
        // The library configures every device while enumerating it.
        if value == self.configuration {
            Ok(())
        } else {
            Err(UsbError::new(UsbErrorKind::Unsupported, Pipe::Device))
        }
    }

    fn active_configuration(&self) -> Option<u8> {
        Some(self.configuration).filter(|&value| value != 0)
    }

    async fn claim_interface(&self, spec: InterfaceSpec) -> Result<(), UsbError> {
        let locate = |endpoint: Option<BulkEndpoint>| -> Result<Option<(BulkEndpoint, usize)>, UsbError> {
            endpoint
                .map(|endpoint| {
                    endpoint_mps(&self.descriptors.config_descriptor, spec.interface, endpoint.address())
                        .map(|mps| (endpoint, mps))
                        .ok_or_else(|| UsbError::new(UsbErrorKind::Fault, Pipe::Bulk(endpoint)))
                })
                .transpose()
        };
        let claim = Claim {
            interface: spec.interface,
            bulk_in: locate(spec.bulk_in)?,
            bulk_out: locate(spec.bulk_out)?,
        };
        // An IDF claim allocates fresh pipes whose data toggles start at DATA0, while the
        // device's endpoints keep theirs; only SET_CONFIGURATION, SET_INTERFACE or a cleared
        // halt resets them. thingino-dfu claims and releases around every operation, which
        // on Linux leaves the toggles alone. Mapped literally onto IDF, the bootrom drops the
        // first packet after every re-claim as a retransmission. So the IDF claim is taken
        // once and kept, and `release_interface` only ends the logical claim.
        if self.idf_claimed.get() != Some(spec.interface) {
            self.release_idf();
            self.claim_idf(spec.interface)?;
        }
        *self.claim.borrow_mut() = Some(claim);
        Ok(())
    }

    async fn release_interface(&self, interface: u8) -> Result<(), UsbError> {
        if self.claimed().is_some_and(|claim| claim.interface == interface) {
            *self.claim.borrow_mut() = None;
        }
        Ok(())
    }

    async fn set_alt_setting(&self, interface: u8, alt: u8) -> Result<(), UsbError> {
        if self.claimed().is_none_or(|claim| claim.interface != interface) {
            return Err(Self::not_claimed(Pipe::Device));
        }
        // SET_INTERFACE. The claim stays on alt 0's endpoints: DFU alts declare none.
        self.control(
            Direction::Out,
            ControlType::Standard,
            Recipient::Interface,
            0x0b,
            u16::from(alt),
            u16::from(interface),
            &[],
            0,
            REQUEST_TIMEOUT,
        )
        .map(drop)
    }

    async fn clear_halt(&self, endpoint: BulkEndpoint) -> Result<(), UsbError> {
        let Some(claim) = self.claimed().filter(|claim| claim.endpoints().any(|declared| declared == endpoint)) else {
            return Err(Self::not_claimed(Pipe::Bulk(endpoint)));
        };
        // CLEAR_FEATURE(ENDPOINT_HALT) puts the device's toggle back to DATA0, and IDF can
        // only do the same for the host by re-claiming, which resets every endpoint of the
        // interface. So every endpoint is cleared on both sides, keeping the pairs in step.
        claim.endpoints().for_each(|declared| self.cancel(declared));
        for declared in claim.endpoints() {
            self.control(
                Direction::Out,
                ControlType::Standard,
                Recipient::Endpoint,
                0x01,
                0,
                u16::from(declared.address()),
                &[],
                0,
                REQUEST_TIMEOUT,
            )?;
        }
        self.release_idf();
        self.claim_idf(claim.interface)
    }

    async fn reset(&self) -> Result<(), UsbError> {
        let client = self.shared.client();
        let old = self.dev();
        self.release_claim();
        self.idf_claimed.set(None);
        // No device-reset call exists in IDF 5.5. Powering the root port off and on bus-resets
        // and re-enumerates the device without cutting VBUS, and it is also the only thing
        // that completes an EP0 transfer the device stopped answering. The handle is closed
        // only once the library reports it gone: before that, such a transfer blocks the close.
        check(unsafe { sys::usb_host_lib_set_root_port_power(false) }, Pipe::Device, "root port off")?;
        self.shared.wait_until(DEVICE_GONE_TIMEOUT, |state| state.gone.contains(&(old as usize)));
        let ep0_inflight = self.ep0_inflight.replace(Arc::new(AtomicUsize::new(0)));
        let drained = Instant::now() + DEVICE_GONE_TIMEOUT;
        while ep0_inflight.load(Ordering::SeqCst) > 0 && Instant::now() < drained {
            thread::sleep(Duration::from_millis(10));
        }
        if ep0_inflight.load(Ordering::SeqCst) == 0 {
            unsafe { sys::usb_host_device_close(client, old) };
            self.shared.forget(old, self.address.get());
        } else {
            self.shared.state.lock().unwrap().deferred.push(Deferred {
                handle: old as usize,
                address: self.address.get(),
                ep0_inflight,
            });
        }
        check(unsafe { sys::usb_host_lib_set_root_port_power(true) }, Pipe::Device, "root port on")?;

        let want = (self.descriptors.vendor_id, self.descriptors.product_id);
        let deadline = Instant::now() + REENUMERATE_TIMEOUT;
        loop {
            let mut addresses = [0u8; 16];
            let mut count = 0;
            unsafe { sys::usb_host_device_addr_list_fill(addresses.len() as i32, addresses.as_mut_ptr(), &mut count) };
            for &address in &addresses[..usize::try_from(count).unwrap_or(0)] {
                let mut dev = ptr::null_mut();
                if unsafe { sys::usb_host_device_open(client, address, &mut dev) } != OK {
                    continue;
                }
                match describe(dev, address) {
                    Ok(described) if (described.descriptors.vendor_id, described.descriptors.product_id) == want => {
                        self.shared.state.lock().unwrap().open.insert(address, described.descriptors);
                        self.dev.set(Raw(dev));
                        self.address.set(address);
                        return Ok(());
                    }
                    _ => unsafe {
                        sys::usb_host_device_close(client, dev);
                    },
                }
            }
            if Instant::now() >= deadline {
                return Err(UsbError::new(UsbErrorKind::NoDevice, Pipe::Device));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn descriptors(&self) -> &DeviceDescriptors {
        &self.descriptors
    }
}
