//! The camera's serial consoles over TCP.
//!
//! Up to three UARTs at 8N1, each one pin driving a chip's RX and one listening to its TX:
//! the camera SoC's always, served raw on [`PORT`] and as RFC 2217 on [`RFC2217_PORT`] (baud
//! rate, break, and DTR/RTS for the camera's boot pin and power, see
//! [`crate::camera::Camera::lines`]), and up to two more for the camera's other chips, a
//! battery or motor MCU, a Wi-Fi chip, raw on the ports after [`PORT`]. The extras take the
//! flash clip's pins, so the flash programmer is off while either is on; on the S3 the second
//! takes UART0 and with it the backpack's own log. One client at a time per UART: a new
//! connection takes it over, so a client that vanished without closing never locks it. With
//! no client connected, a UART's output is read and dropped.
//!
//! Which UARTs are on, their names and their baud rates are settings, kept in NVS. Turning
//! one on or off takes a restart, as the pins are handed out at boot.
//!
//! One thread serves it all from `poll()`: ESP-IDF's VFS can wait on a UART with a driver
//! installed and on lwIP sockets together.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use core::time::Duration;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::{Arc, OnceLock};

use esp_idf_svc::hal::gpio::{self, InputPin, OutputPin};
use esp_idf_svc::hal::uart::config::{Config, DataBits, Parity, StopBits};
use esp_idf_svc::hal::uart::{Uart, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys;
use log::{info, warn};

use crate::camera::Camera;
use crate::rfc2217::{Port, Session, IAC};

/// UART 1's raw port; UART 2's and 3's are the next two.
pub const PORT: u16 = 3000;
pub const RFC2217_PORT: u16 = 2217;

/// How many UARTs the chip has room for: the S2 has two in all, one of them the log's.
#[cfg(esp32s2)]
pub const LINES: usize = 2;
#[cfg(not(esp32s2))]
pub const LINES: usize = 3;

/// Each UART's TX and RX GPIOs, as `main` hands them out: UART 2 on the clip's CS and DO,
/// UART 3 on its CLK and VCC.
#[cfg(not(esp32p4))]
pub const PINS: [(i32, i32); 3] = [(1, 2), (5, 8), (7, 3)];
#[cfg(esp32p4)]
pub const PINS: [(i32, i32); 3] = [(22, 23), (45, 47), (46, 32)];

const BAUD: u32 = 115_200;

/// About 90 ms of a chip talking flat out, which covers the longest the thread can be away
/// from a UART: one chunk of client input going out at the same rate.
const RX_RING: usize = 1024;

/// Client input goes to a UART a hardware FIFO at a time, so writing it never keeps the
/// thread from the UARTs' input for more than about 11 ms.
const INPUT_CHUNK: usize = 128;

/// A client that takes no output for this long is dropped rather than left to stall the
/// console.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// The console thread's FreeRTOS handle, for the memory report's stack high-water mark.
pub static TASK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(core::ptr::null_mut());

/// Each running UART's TX pin and port, -1 for one that is off, and whether TX should drive
/// the chips' RX, which the camera module decides.
static TX_PINS: [AtomicI32; 3] = [AtomicI32::new(-1), AtomicI32::new(-1), AtomicI32::new(-1)];
static UART_PORTS: [AtomicI32; 3] = [AtomicI32::new(-1), AtomicI32::new(-1), AtomicI32::new(-1)];
static RX_PIN: AtomicI32 = AtomicI32::new(-1);
static TX_WANTED: AtomicBool = AtomicBool::new(false);
/// Each UART's baud rate setting, which a client's session goes back to.
static BAUDS: [AtomicU32; 3] = [AtomicU32::new(BAUD), AtomicU32::new(BAUD), AtomicU32::new(BAUD)];
/// The rate UART 1 was last set to, which RFC 2217 answers with: pyserial compares the
/// answer with its request byte for byte, and the UART's own read-back is rounded.
static BAUD_SET: AtomicU32 = AtomicU32::new(BAUD);

/// Where the settings are kept: `on1` and `on2` for UART 2 and 3, `name0` to `name2`, and
/// `baud0` to `baud2`.
const NAMESPACE: &str = "console";
const NAME_MAX: usize = 24;
const DEFAULT_NAMES: [&str; 3] = ["Camera SoC", "UART 2", "UART 3"];
static NVS: OnceLock<EspDefaultNvsPartition> = OnceLock::new();

/// The UARTs' settings.
pub struct Settings {
    pub on: [bool; 3],
    pub names: [String; 3],
    pub bauds: [u32; 3],
}

impl Settings {
    /// Whether either extra UART is on, which takes the flash clip's pins.
    pub fn extras(&self) -> bool {
        self.on[1..LINES].iter().any(|&on| on)
    }
}

/// Reads the settings, and keeps `nvs` for changing them.
pub fn settings(nvs: &EspDefaultNvsPartition) -> Settings {
    let _ = NVS.set(nvs.clone());
    load()
}

fn load() -> Settings {
    let mut settings = Settings {
        on: [true, false, false],
        names: DEFAULT_NAMES.map(str::to_owned),
        bauds: [BAUD; 3],
    };
    let Some(store) = NVS.get().and_then(|nvs| EspNvs::new(nvs.clone(), NAMESPACE, true).ok()) else {
        return settings;
    };
    let mut buf = [0u8; NAME_MAX * 4 + 1];
    for line in 0..LINES {
        if line > 0 {
            settings.on[line] = store.get_u8(&format!("on{line}")).ok().flatten().is_some_and(|on| on != 0);
        }
        if let Ok(Some(name)) = store.get_str(&format!("name{line}"), &mut buf) {
            settings.names[line] = name.to_owned();
        }
        if let Ok(Some(baud)) = store.get_u32(&format!("baud{line}")) {
            settings.bauds[line] = baud;
        }
    }
    settings
}

fn store() -> Result<EspNvs<NvsDefault>, String> {
    let nvs = NVS.get().ok_or("console settings: no NVS")?;
    EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))
}

fn line_index(line: usize) -> Result<usize, String> {
    (line < LINES).then_some(line).ok_or_else(|| format!("uart must be 0 to {}", LINES - 1))
}

/// Turns UART 2 or 3 on or off from the next boot on, and restarts into it.
pub fn set_on(line: usize, on: bool) -> Result<String, String> {
    let line = line_index(line)?;
    if line == 0 {
        return Err("UART 1, the camera SoC's, is always on".into());
    }
    store()?.set_u8(&format!("on{line}"), u8::from(on)).map_err(|err| err.to_string())?;
    crate::restart_soon();
    Ok(format!("UART {} {}; restarting", line + 1, if on { "on" } else { "off" }))
}

pub fn set_name(line: usize, name: &str) -> Result<String, String> {
    let line = line_index(line)?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > NAME_MAX || name.chars().any(char::is_control) {
        return Err(format!("a name is 1 to {NAME_MAX} characters"));
    }
    store()?.set_str(&format!("name{line}"), name).map_err(|err| err.to_string())?;
    Ok(format!("UART {} is {name}", line + 1))
}

/// Sets a UART's baud rate, now if it is on, and on every boot after.
pub fn set_baud(line: usize, baud: u32) -> Result<String, String> {
    let line = line_index(line)?;
    if !(300..=5_000_000).contains(&baud) {
        return Err("baud must be 300 to 5000000".into());
    }
    store()?.set_u32(&format!("baud{line}"), baud).map_err(|err| err.to_string())?;
    BAUDS[line].store(baud, Ordering::Relaxed);
    if let Ok(port) = sys::uart_port_t::try_from(UART_PORTS[line].load(Ordering::SeqCst)) {
        // SAFETY: the driver is installed for as long as the port is recorded.
        sys::esp!(unsafe { sys::uart_set_baudrate(port, baud) }).map_err(|err| err.to_string())?;
        if line == 0 {
            BAUD_SET.store(baud, Ordering::Relaxed);
        }
    }
    Ok(format!("UART {} at {baud} baud", line + 1))
}

/// The UARTs as the status page shows them, as JSON.
pub fn json() -> String {
    let settings = load();
    let uarts: Vec<String> = (0..LINES)
        .map(|line| {
            format!(
                r#"{{"on":{},"name":"{}","baud":{},"tx":{},"rx":{},"port":{}}}"#,
                settings.on[line],
                settings.names[line].replace('\\', "\\\\").replace('"', "\\\""),
                settings.bauds[line],
                PINS[line].0,
                PINS[line].1,
                PORT + line as u16
            )
        })
        .collect();
    format!(r#"{{"uarts":[{}]}}"#, uarts.join(","))
}

/// UART 1's TX and RX GPIOs, once it is started.
pub fn pins() -> (i32, i32) {
    (TX_PINS[0].load(Ordering::Relaxed), RX_PIN.load(Ordering::Relaxed))
}

/// Silences the backpack's own log, for the UART it goes out on to serve a camera's chip.
/// The P4's extra UARTs leave the log's alone.
#[cfg(not(esp32p4))]
pub fn quiet_log() {
    unsafe extern "C" fn quiet(_: *const core::ffi::c_char, _: sys::va_list) -> core::ffi::c_int {
        0
    }
    // SAFETY: a log level and an output function, both of which ESP-IDF takes at any time.
    unsafe {
        sys::esp_log_level_set(c"*".as_ptr(), sys::esp_log_level_t_ESP_LOG_NONE);
        sys::esp_log_set_vprintf(Some(quiet));
    }
}

/// Drives the chips' RX from the UARTs' TX, or leaves them floating. A powered-off chip's RX
/// must float: driven high, it back-powers the chip through its input protection and some
/// SoCs then fail to cold boot.
pub fn set_tx(wanted: bool) {
    TX_WANTED.store(wanted, Ordering::SeqCst);
    apply_tx();
}

/// Whether the camera's UART TX holds the line high, as an idle UART does whenever the
/// camera has power. Sampled for up to 20 ms, so bytes in flight still show a high bit; an
/// unpowered camera clamps the line low.
pub fn camera_tx_high() -> bool {
    let pin = RX_PIN.load(Ordering::SeqCst);
    if pin < 0 {
        return false;
    }
    (0..20).any(|_| {
        std::thread::sleep(Duration::from_millis(1));
        // SAFETY: reading a pin's input level changes nothing, whoever drives it.
        let level = unsafe { sys::gpio_get_level(pin) };
        level == 1
    })
}

fn apply_tx() {
    for (pin, port) in TX_PINS.iter().zip(&UART_PORTS) {
        let pin = pin.load(Ordering::SeqCst);
        let Ok(port) = sys::uart_port_t::try_from(port.load(Ordering::SeqCst)) else {
            continue;
        };
        if pin < 0 {
            continue;
        }
        if TX_WANTED.load(Ordering::SeqCst) {
            // SAFETY: the driver is installed; this routes its TX signal back to the pin and
            // leaves RX as it is.
            unsafe { sys::uart_set_pin(port, pin, -1, -1, -1) };
        } else {
            // SAFETY: the pin is the console's. Resetting it disconnects the UART and turns
            // the output off; the pull-up the reset enables goes too.
            unsafe {
                sys::gpio_reset_pin(pin);
                sys::gpio_pullup_dis(pin);
            }
        }
    }
}

/// One UART, set up and waiting for [`start`].
pub struct Line {
    index: usize,
    uart: UartDriver<'static>,
    ready: File,
}

/// Installs UART `index` (0 to 2) at `baud`, its TX floating until the camera is up.
pub fn line(
    index: usize,
    uart: impl Uart + 'static,
    tx: impl OutputPin + 'static,
    rx: impl InputPin + 'static,
    baud: u32,
) -> Result<Line, String> {
    let tx_pin = i32::from(tx.pin());
    let rx_pin = i32::from(rx.pin());
    // No transmit ring: a write returns once its bytes are in the hardware FIFO, which the
    // input chunking keeps short. No event queue: poll() is woken by the driver directly.
    let config = Config::new().baudrate(Hertz(baud)).rx_fifo_size(RX_RING).tx_fifo_size(0).queue_size(0);
    let uart = UartDriver::new(uart, tx, rx, Option::<gpio::AnyIOPin>::None, Option::<gpio::AnyIOPin>::None, &config)
        .map_err(|err| format!("UART {}: {err}", index + 1))?;
    BAUDS[index].store(baud, Ordering::Relaxed);
    if index == 0 {
        RX_PIN.store(rx_pin, Ordering::Relaxed);
        BAUD_SET.store(baud, Ordering::Relaxed);
    }
    UART_PORTS[index].store(i32::try_from(uart.port()).unwrap_or(-1), Ordering::SeqCst);
    TX_PINS[index].store(tx_pin, Ordering::SeqCst);
    apply_tx();
    // Opened only for poll(); the bytes themselves go through the driver.
    let ready = File::open(format!("/dev/uart/{}", uart.port())).map_err(|err| format!("UART {}: {err}", index + 1))?;
    Ok(Line { index, uart, ready })
}

/// Serves `lines`, UART 1 first.
pub fn start(lines: Vec<Line>, camera: Arc<Camera>) -> Result<(), String> {
    let mut consoles = Vec::with_capacity(lines.len());
    for line in lines {
        let port = PORT + line.index as u16;
        let raw = TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)).map_err(|err| format!("console: {err}"))?;
        let rfc2217 = if line.index == 0 {
            Some(TcpListener::bind((Ipv6Addr::UNSPECIFIED, RFC2217_PORT)).map_err(|err| format!("console: {err}"))?)
        } else {
            None
        };
        info!(
            "console: UART {} (UART{}) raw on port {port}{}",
            line.index + 1,
            line.uart.port(),
            if rfc2217.is_some() { format!(", RFC 2217 on {RFC2217_PORT}") } else { String::new() }
        );
        consoles.push(Console { line, raw, rfc2217, client: None });
    }
    std::thread::Builder::new()
        .name("console".into())
        .stack_size(5 * 1024)
        .spawn(move || {
            TASK.store(unsafe { sys::xTaskGetCurrentTaskHandle() }.cast(), Ordering::Relaxed);
            serve(&mut consoles, &camera);
        })
        .map_err(|err| format!("console: {err}"))?;
    Ok(())
}

struct Console {
    line: Line,
    raw: TcpListener,
    /// UART 1's alone.
    rfc2217: Option<TcpListener>,
    client: Option<Client>,
}

struct Client {
    stream: TcpStream,
    /// The telnet session of an RFC 2217 client; a raw client has none.
    session: Option<Session>,
}

fn serve(consoles: &mut [Console], camera: &Camera) -> ! {
    let mut output = [0u8; 512];
    let mut input = [0u8; INPUT_CHUNK];
    let mut data = Vec::with_capacity(INPUT_CHUNK);
    let mut reply = Vec::with_capacity(64);
    let mut fds = Vec::with_capacity(consoles.len() * 4);
    loop {
        // Four descriptors per console, in this order; a missing one is -1, which poll()
        // skips.
        fds.clear();
        for console in consoles.iter() {
            fds.push(waiting_for_input(console.line.ready.as_raw_fd()));
            fds.push(waiting_for_input(console.client.as_ref().map_or(-1, |client| client.stream.as_raw_fd())));
            fds.push(waiting_for_input(console.raw.as_raw_fd()));
            fds.push(waiting_for_input(console.rfc2217.as_ref().map_or(-1, AsRawFd::as_raw_fd)));
        }
        if unsafe { sys::poll(fds.as_mut_ptr(), fds.len() as sys::nfds_t, -1) } < 0 {
            warn!("console: poll: {}", io::Error::last_os_error());
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut progressed = false;
        for (console, ready) in consoles.iter_mut().zip(fds.chunks(4)) {
            progressed |= serve_one(console, ready, camera, &mut output, &mut input, &mut data, &mut reply);
        }
        // A descriptor poll() keeps reporting while nothing can be done with it would
        // otherwise spin this thread and starve the idle task.
        if !progressed {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

/// One console's share of a poll() round; whether anything was done.
fn serve_one(
    console: &mut Console,
    ready: &[sys::pollfd],
    camera: &Camera,
    output: &mut [u8],
    input: &mut [u8],
    data: &mut Vec<u8>,
    reply: &mut Vec<u8>,
) -> bool {
    let Console { line, raw, rfc2217, client } = console;
    let uart = &line.uart;
    let index = line.index;
    let mut progressed = false;

    if ready[0].revents != 0 {
        if let Ok(read) = uart.read(output, 0) {
            progressed = true;
            if let Some(active) = client.as_mut() {
                if let Err(err) = send(active, &output[..read]) {
                    info!("console {}: client dropped ({err})", index + 1);
                    end(client.take(), line, camera);
                }
            }
        }
    }

    if ready[1].revents != 0 {
        if let Some(active) = client.as_mut() {
            progressed = true;
            match active.stream.read(input) {
                Ok(0) => {
                    info!("console {}: client left", index + 1);
                    end(client.take(), line, camera);
                }
                Ok(read) => {
                    let bytes = &input[..read];
                    let to_uart = if let Some(session) = active.session.as_mut() {
                        data.clear();
                        reply.clear();
                        session.input(bytes, data, reply, &mut UartPort { uart, camera });
                        if !reply.is_empty() && active.stream.write_all(reply).is_err() {
                            info!("console {}: client dropped while answering", index + 1);
                            end(client.take(), line, camera);
                            return true;
                        }
                        &data[..]
                    } else {
                        bytes
                    };
                    if !to_uart.is_empty() {
                        if let Err(err) = uart.write(to_uart) {
                            warn!("console {}: UART write: {err}", index + 1);
                        }
                    }
                }
                Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                Err(err) => {
                    info!("console {}: client dropped ({err})", index + 1);
                    end(client.take(), line, camera);
                }
            }
        }
    }

    for (listener, telnet, at) in [(Some(&*raw), false, 2), (rfc2217.as_ref(), true, 3)] {
        let Some(listener) = listener else {
            continue;
        };
        if ready[at].revents == 0 {
            continue;
        }
        progressed = true;
        match listener.accept() {
            Ok((stream, peer)) => {
                if let Some(mut old) = client.take() {
                    let _ = send(&mut old, format!("\r\n[console taken over by {peer}]\r\n").as_bytes());
                    info!("console {}: {peer} takes over", index + 1);
                    end(Some(old), line, camera);
                } else {
                    info!("console {}: {peer} connected{}", index + 1, if telnet { " (RFC 2217)" } else { "" });
                }
                *client = connect(stream, peer, telnet);
            }
            Err(err) => warn!("console {}: accept: {err}", index + 1),
        }
    }
    progressed
}

fn connect(mut stream: TcpStream, peer: SocketAddr, telnet: bool) -> Option<Client> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(SEND_TIMEOUT));
    let session = if telnet {
        let mut offers = Vec::new();
        let session = Session::open(&mut offers);
        if let Err(err) = stream.write_all(&offers) {
            info!("console: {peer} dropped ({err})");
            return None;
        }
        Some(session)
    } else {
        None
    };
    Some(Client { stream, session })
}

/// A UART's bytes to the client; an RFC 2217 client gets 0xFF doubled, as telnet frames it.
fn send(client: &mut Client, bytes: &[u8]) -> io::Result<()> {
    if client.session.is_none() {
        return client.stream.write_all(bytes);
    }
    for (index, part) in bytes.split(|&byte| byte == IAC).enumerate() {
        if index > 0 {
            client.stream.write_all(&[IAC, IAC])?;
        }
        client.stream.write_all(part)?;
    }
    Ok(())
}

/// What an RFC 2217 client leaves behind is undone: the lines go idle, a break ends, and the
/// UART is back at its own baud rate and 8N1 for whoever comes next.
fn end(client: Option<Client>, line: &Line, camera: &Camera) {
    let Some(session) = client.and_then(|client| client.session) else {
        return;
    };
    let uart = &line.uart;
    let mut port = UartPort { uart, camera };
    port.set_lines(true, true);
    port.set_break(false);
    if session.changed {
        let baud = BAUDS[line.index].load(Ordering::Relaxed);
        BAUD_SET.store(baud, Ordering::Relaxed);
        let _ = uart
            .change_baudrate(Hertz(baud))
            .and_then(|uart| uart.change_data_bits(DataBits::DataBits8))
            .and_then(|uart| uart.change_parity(Parity::ParityNone))
            .and_then(|uart| uart.change_stop_bits(StopBits::STOP1));
    }
}

/// The UART and the camera, as RFC 2217 requests reach them.
struct UartPort<'a> {
    uart: &'a UartDriver<'a>,
    camera: &'a Camera,
}

impl Port for UartPort<'_> {
    fn baudrate(&self) -> u32 {
        BAUD_SET.load(Ordering::Relaxed)
    }

    fn set_baudrate(&mut self, baud: u32) {
        match self.uart.change_baudrate(Hertz(baud)) {
            Ok(_) => BAUD_SET.store(baud, Ordering::Relaxed),
            Err(err) => warn!("console: baud rate {baud}: {err}"),
        }
    }

    fn datasize(&self) -> u8 {
        match self.uart.data_bits() {
            Ok(DataBits::DataBits5) => 5,
            Ok(DataBits::DataBits6) => 6,
            Ok(DataBits::DataBits7) => 7,
            _ => 8,
        }
    }

    fn set_datasize(&mut self, bits: u8) {
        let bits = match bits {
            5 => DataBits::DataBits5,
            6 => DataBits::DataBits6,
            7 => DataBits::DataBits7,
            _ => DataBits::DataBits8,
        };
        let _ = self.uart.change_data_bits(bits);
    }

    fn parity(&self) -> u8 {
        match self.uart.parity() {
            Ok(Parity::ParityOdd) => 2,
            Ok(Parity::ParityEven) => 3,
            _ => 1,
        }
    }

    fn set_parity(&mut self, parity: u8) {
        let parity = match parity {
            2 => Parity::ParityOdd,
            3 => Parity::ParityEven,
            _ => Parity::ParityNone,
        };
        let _ = self.uart.change_parity(parity);
    }

    fn stopsize(&self) -> u8 {
        match self.uart.stop_bits() {
            Ok(StopBits::STOP2) => 2,
            Ok(StopBits::STOP1P5) => 3,
            _ => 1,
        }
    }

    fn set_stopsize(&mut self, stop: u8) {
        let stop = match stop {
            2 => StopBits::STOP2,
            3 => StopBits::STOP1P5,
            _ => StopBits::STOP1,
        };
        let _ = self.uart.change_stop_bits(stop);
    }

    fn set_break(&mut self, on: bool) {
        // An inverted TX idles low, which is a break for as long as it lasts.
        let mask = if on {
            sys::uart_signal_inv_t_UART_SIGNAL_TXD_INV
        } else {
            sys::uart_signal_inv_t_UART_SIGNAL_INV_DISABLE
        };
        // SAFETY: the driver is installed.
        unsafe { sys::uart_set_line_inverse(self.uart.port(), mask) };
    }

    fn set_lines(&mut self, dtr: bool, rts: bool) {
        self.camera.lines(dtr, rts);
    }

    fn purge_input(&mut self) {
        // SAFETY: the driver is installed.
        unsafe { sys::uart_flush_input(self.uart.port()) };
    }
}

fn waiting_for_input(fd: i32) -> sys::pollfd {
    sys::pollfd {
        fd,
        events: sys::POLLIN as i16,
        revents: 0,
    }
}
