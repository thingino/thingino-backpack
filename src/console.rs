//! The camera's serial console over TCP.
//!
//! UART1 at 115200 8N1, GPIO17 driving the camera's RX and GPIO18 listening to its TX,
//! served raw on [`PORT`] and as RFC 2217 on [`RFC2217_PORT`] (baud rate, break, and DTR/RTS
//! for the camera's boot pin and power, see [`crate::camera::Camera::lines`]). One client at
//! a time across both: a new connection takes the console over, so a client that vanished
//! without closing never locks it. With no client connected, the camera's output is read
//! and dropped.
//!
//! One thread serves it all from `poll()`: ESP-IDF's VFS can wait on a UART with a driver
//! installed and on lwIP sockets together.

use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU32, Ordering};
use core::time::Duration;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::Arc;

use esp_idf_svc::hal::gpio::{self, InputPin, OutputPin};
use esp_idf_svc::hal::uart::config::{Config, DataBits, Parity, StopBits};
use esp_idf_svc::hal::uart::{Uart, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys;
use log::{info, warn};

use crate::camera::Camera;
use crate::rfc2217::{Port, Session, IAC};

pub const PORT: u16 = 3000;
pub const RFC2217_PORT: u16 = 2217;

const BAUD: u32 = 115_200;

/// About 90 ms of the camera talking flat out, which covers the longest the thread can be
/// away from the UART: one chunk of client input going out at the same rate.
const RX_RING: usize = 1024;

/// Client input goes to the UART a hardware FIFO at a time, so writing it never keeps the
/// thread from the UART's input for more than about 11 ms.
const INPUT_CHUNK: usize = 128;

/// A client that takes no output for this long is dropped rather than left to stall the
/// console.
const SEND_TIMEOUT: Duration = Duration::from_secs(2);

/// The console thread's FreeRTOS handle, for the memory report's stack high-water mark.
pub static TASK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(core::ptr::null_mut());

/// The TX pin and UART once `start` has installed them, and whether TX should drive the
/// camera's RX, which the camera module decides.
static TX_PIN: AtomicI32 = AtomicI32::new(-1);
static RX_PIN: AtomicI32 = AtomicI32::new(-1);
/// The rate the UART was last set to, which RFC 2217 answers with: pyserial compares the
/// answer with its request byte for byte, and the UART's own read-back is rounded.
static BAUD_SET: AtomicU32 = AtomicU32::new(BAUD);
static UART_PORT: AtomicI32 = AtomicI32::new(-1);
static TX_WANTED: AtomicBool = AtomicBool::new(false);

/// The console's TX and RX GPIOs, once it is started.
pub fn pins() -> (i32, i32) {
    (TX_PIN.load(Ordering::Relaxed), RX_PIN.load(Ordering::Relaxed))
}

/// Drives the camera's RX from TX, or leaves it floating. A powered-off camera's RX must
/// float: driven high, it back-powers the camera through its input protection and some
/// SoCs then fail to cold boot.
pub fn set_tx(wanted: bool) {
    TX_WANTED.store(wanted, Ordering::SeqCst);
    apply_tx();
}

fn apply_tx() {
    let pin = TX_PIN.load(Ordering::SeqCst);
    let Ok(port) = sys::uart_port_t::try_from(UART_PORT.load(Ordering::SeqCst)) else {
        return;
    };
    if pin < 0 {
        return;
    }
    if TX_WANTED.load(Ordering::SeqCst) {
        // SAFETY: the driver is installed; this routes its TX signal back to the pin and
        // leaves RX as it is.
        unsafe { sys::uart_set_pin(port, pin, -1, -1, -1) };
    } else {
        // SAFETY: the pin is the console's. Resetting it disconnects the UART and turns the
        // output off; the pull-up the reset enables goes too.
        unsafe {
            sys::gpio_reset_pin(pin);
            sys::gpio_pullup_dis(pin);
        }
    }
}

pub fn start(
    uart: impl Uart + 'static,
    tx: impl OutputPin + 'static,
    rx: impl InputPin + 'static,
    camera: Arc<Camera>,
) -> Result<(), String> {
    let tx_pin = i32::from(tx.pin());
    RX_PIN.store(i32::from(rx.pin()), Ordering::Relaxed);
    // No transmit ring: a write returns once its bytes are in the hardware FIFO, which the
    // input chunking keeps short. No event queue: poll() is woken by the driver directly.
    let config = Config::new().baudrate(Hertz(BAUD)).rx_fifo_size(RX_RING).tx_fifo_size(0).queue_size(0);
    let uart = UartDriver::new(uart, tx, rx, Option::<gpio::AnyIOPin>::None, Option::<gpio::AnyIOPin>::None, &config)
        .map_err(|err| format!("console UART: {err}"))?;
    UART_PORT.store(i32::try_from(uart.port()).unwrap_or(-1), Ordering::SeqCst);
    TX_PIN.store(tx_pin, Ordering::SeqCst);
    // TX stays off until the camera module says the camera is up.
    apply_tx();
    // Opened only for poll(); the bytes themselves go through the driver.
    let ready = File::open(format!("/dev/uart/{}", uart.port())).map_err(|err| format!("console UART: {err}"))?;
    let raw = TcpListener::bind((Ipv6Addr::UNSPECIFIED, PORT)).map_err(|err| format!("console: {err}"))?;
    let rfc2217 = TcpListener::bind((Ipv6Addr::UNSPECIFIED, RFC2217_PORT)).map_err(|err| format!("console: {err}"))?;
    info!("console: UART{} raw on port {PORT}, RFC 2217 on {RFC2217_PORT}", uart.port());
    std::thread::Builder::new()
        .name("console".into())
        .stack_size(5 * 1024)
        .spawn(move || {
            TASK.store(unsafe { sys::xTaskGetCurrentTaskHandle() }.cast(), Ordering::Relaxed);
            serve(&uart, &ready, [&raw, &rfc2217], &camera);
        })
        .map_err(|err| format!("console: {err}"))?;
    Ok(())
}

struct Client {
    stream: TcpStream,
    /// The telnet session of an RFC 2217 client; a raw client has none.
    session: Option<Session>,
}

fn serve(uart: &UartDriver<'_>, ready: &File, listeners: [&TcpListener; 2], camera: &Camera) -> ! {
    let mut client: Option<Client> = None;
    let mut output = [0u8; 512];
    let mut input = [0u8; INPUT_CHUNK];
    let mut data = Vec::with_capacity(INPUT_CHUNK);
    let mut reply = Vec::with_capacity(64);
    loop {
        let mut fds = [
            waiting_for_input(ready.as_raw_fd()),
            waiting_for_input(client.as_ref().map_or(-1, |client| client.stream.as_raw_fd())),
            waiting_for_input(listeners[0].as_raw_fd()),
            waiting_for_input(listeners[1].as_raw_fd()),
        ];
        if unsafe { sys::poll(fds.as_mut_ptr(), fds.len() as sys::nfds_t, -1) } < 0 {
            warn!("console: poll: {}", io::Error::last_os_error());
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut progressed = false;

        if fds[0].revents != 0 {
            if let Ok(read) = uart.read(&mut output, 0) {
                progressed = true;
                if let Some(active) = client.as_mut() {
                    if let Err(err) = send(active, &output[..read]) {
                        info!("console: client dropped ({err})");
                        end(client.take(), uart, camera);
                    }
                }
            }
        }

        if fds[1].revents != 0 {
            if let Some(active) = client.as_mut() {
                progressed = true;
                match active.stream.read(&mut input) {
                    Ok(0) => {
                        info!("console: client left");
                        end(client.take(), uart, camera);
                    }
                    Ok(read) => {
                        let bytes = &input[..read];
                        let to_uart = if let Some(session) = active.session.as_mut() {
                            data.clear();
                            reply.clear();
                            session.input(bytes, &mut data, &mut reply, &mut UartPort { uart, camera });
                            if !reply.is_empty() && active.stream.write_all(&reply).is_err() {
                                info!("console: client dropped while answering");
                                end(client.take(), uart, camera);
                                continue;
                            }
                            &data[..]
                        } else {
                            bytes
                        };
                        if !to_uart.is_empty() {
                            if let Err(err) = uart.write(to_uart) {
                                warn!("console: UART write: {err}");
                            }
                        }
                    }
                    Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                    Err(err) => {
                        info!("console: client dropped ({err})");
                        end(client.take(), uart, camera);
                    }
                }
            }
        }

        for (listener, telnet) in [(listeners[0], false), (listeners[1], true)] {
            let index = if telnet { 3 } else { 2 };
            if fds[index].revents == 0 {
                continue;
            }
            progressed = true;
            match listener.accept() {
                Ok((stream, peer)) => {
                    if let Some(mut old) = client.take() {
                        let _ = send(&mut old, format!("\r\n[console taken over by {peer}]\r\n").as_bytes());
                        info!("console: {peer} takes over");
                        end(Some(old), uart, camera);
                    } else {
                        info!("console: {peer} connected{}", if telnet { " (RFC 2217)" } else { "" });
                    }
                    client = connect(stream, peer, telnet);
                }
                Err(err) => warn!("console: accept: {err}"),
            }
        }

        // A descriptor poll() keeps reporting while nothing can be done with it would
        // otherwise spin this thread and starve the idle task.
        if !progressed {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
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

/// The camera's bytes to the client; an RFC 2217 client gets 0xFF doubled, as telnet
/// frames it.
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

/// What a client leaves behind is undone: the lines go idle, a break ends, and the UART is
/// back at 115200 8N1 for whoever comes next.
fn end(client: Option<Client>, uart: &UartDriver<'_>, camera: &Camera) {
    let Some(session) = client.and_then(|client| client.session) else {
        return;
    };
    let mut port = UartPort { uart, camera };
    port.set_lines(true, true);
    port.set_break(false);
    if session.changed {
        BAUD_SET.store(BAUD, Ordering::Relaxed);
        let _ = uart
            .change_baudrate(Hertz(BAUD))
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
