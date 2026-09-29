//! The camera's serial console over TCP.
//!
//! UART1 at 115200 8N1, GPIO17 driving the camera's RX and GPIO18 listening to its TX,
//! bridged raw to [`PORT`]. One client at a time: a new connection takes the console over,
//! so a client that vanished without closing never locks it. With no client connected,
//! the camera's output is read and dropped.
//!
//! One thread serves it all from `poll()`: ESP-IDF's VFS can wait on a UART with a driver
//! installed and on lwIP sockets together.
//!
//! GPIO17 idles high whether or not the camera has power. Driving a powered-off camera's RX
//! can back-power it and keep some SoCs from cold booting; gating TX on camera power
//! belongs with the power switch.

use core::sync::atomic::{AtomicPtr, Ordering};
use core::time::Duration;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv6Addr, TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use esp_idf_svc::hal::gpio::{self, InputPin, OutputPin};
use esp_idf_svc::hal::uart::{config::Config, Uart, UartDriver};
use esp_idf_svc::hal::units::Hertz;
use esp_idf_svc::sys;
use log::{info, warn};

pub const PORT: u16 = 3000;

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

pub fn start(uart: impl Uart + 'static, tx: impl OutputPin + 'static, rx: impl InputPin + 'static) -> Result<(), String> {
    // No transmit ring: a write returns once its bytes are in the hardware FIFO, which the
    // input chunking keeps short. No event queue: poll() is woken by the driver directly.
    let config = Config::new().baudrate(Hertz(BAUD)).rx_fifo_size(RX_RING).tx_fifo_size(0).queue_size(0);
    let uart = UartDriver::new(uart, tx, rx, Option::<gpio::AnyIOPin>::None, Option::<gpio::AnyIOPin>::None, &config)
        .map_err(|err| format!("console UART: {err}"))?;
    // Opened only for poll(); the bytes themselves go through the driver.
    let ready = File::open(format!("/dev/uart/{}", uart.port())).map_err(|err| format!("console UART: {err}"))?;
    let listener = TcpListener::bind((Ipv6Addr::UNSPECIFIED, PORT)).map_err(|err| format!("console: {err}"))?;
    info!("console: UART{} on port {PORT}", uart.port());
    std::thread::Builder::new()
        .name("console".into())
        .stack_size(5 * 1024)
        .spawn(move || {
            TASK.store(unsafe { sys::xTaskGetCurrentTaskHandle() }.cast(), Ordering::Relaxed);
            serve(&uart, &ready, &listener);
        })
        .map_err(|err| format!("console: {err}"))?;
    Ok(())
}

fn serve(uart: &UartDriver<'_>, ready: &File, listener: &TcpListener) -> ! {
    let mut client: Option<TcpStream> = None;
    let mut output = [0u8; 512];
    let mut input = [0u8; INPUT_CHUNK];
    loop {
        let mut fds = [
            waiting_for_input(ready.as_raw_fd()),
            waiting_for_input(client.as_ref().map_or(-1, AsRawFd::as_raw_fd)),
            waiting_for_input(listener.as_raw_fd()),
        ];
        if unsafe { sys::poll(fds.as_mut_ptr(), fds.len() as sys::nfds_t, -1) } < 0 {
            warn!("console: poll: {}", std::io::Error::last_os_error());
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        let mut progressed = false;

        if fds[0].revents != 0 {
            if let Ok(read) = uart.read(&mut output, 0) {
                progressed = true;
                if let Some(stream) = client.as_mut() {
                    if let Err(err) = stream.write_all(&output[..read]) {
                        info!("console: client dropped ({err})");
                        client = None;
                    }
                }
            }
        }

        if fds[1].revents != 0 {
            if let Some(stream) = client.as_mut() {
                progressed = true;
                match stream.read(&mut input) {
                    Ok(0) => {
                        info!("console: client left");
                        client = None;
                    }
                    Ok(read) => {
                        if let Err(err) = uart.write(&input[..read]) {
                            warn!("console: UART write: {err}");
                        }
                    }
                    Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                    Err(err) => {
                        info!("console: client dropped ({err})");
                        client = None;
                    }
                }
            }
        }

        if fds[2].revents != 0 {
            progressed = true;
            match listener.accept() {
                Ok((stream, peer)) => {
                    if let Some(mut old) = client.take() {
                        let _ = old.write_all(format!("\r\n[console taken over by {peer}]\r\n").as_bytes());
                        info!("console: {peer} takes over");
                    } else {
                        info!("console: {peer} connected");
                    }
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_write_timeout(Some(SEND_TIMEOUT));
                    client = Some(stream);
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

fn waiting_for_input(fd: i32) -> sys::pollfd {
    sys::pollfd {
        fd,
        events: sys::POLLIN as i16,
        revents: 0,
    }
}
