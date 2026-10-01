//! thingino-backpack: the thingino-dfu daemon (`dfu-remote`) on an ESP32-S3, -S2 or -P4.
//!
//! The same tdfu-daemon library a Linux host runs, served over Wi-Fi (Ethernet on the P4,
//! which has no radio), with the camera on the OTG port driven by the ESP-IDF USB host
//! backend. `thingino-dfu --host <backpack>` and the browser flasher's remote mode talk to it
//! unchanged.

use core::sync::atomic::{AtomicPtr, Ordering};
use core::time::Duration;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::thread::ThreadSpawnConfiguration;
use esp_idf_svc::io::vfs::MountedEventfs;
#[cfg(not(esp32p4))]
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use log::{error, info};
use tdfu_daemon::auth::Auth;
use tdfu_daemon::commands::state::{DaemonState, ReadStaging};
use tdfu_daemon::serve::{serve, Signals};
use tdfu_daemon::transport::{Origins, Timeouts};
use tdfu_daemon::{listen, TokioClock, DEFAULT_PORT};

use tdfu_usb::espidf::UsbHost;

mod camera;
mod console;
#[cfg(esp32p4)]
mod eth;
#[cfg(not(esp32p4))]
mod portal;
mod rfc2217;
mod serprog;
mod status;
#[cfg(not(esp32p4))]
mod wifi;

/// The interface clients reach the unit on, whose addresses the status page lists.
#[cfg(not(esp32p4))]
const NETIF_KEY: &core::ffi::CStr = wifi::NETIF_KEY;
#[cfg(esp32p4)]
const NETIF_KEY: &core::ffi::CStr = eth::NETIF_KEY;

/// Where the daemon would look for loaders. Deliberately empty: the client streams the
/// loader pair with BOOTSTRAP, so nothing is stored on the unit.
const NO_FIRMWARE_DIR: &str = "/no-loaders";

/// Request payloads over this are streamed rather than held. A streamed BOOTSTRAP still
/// holds its stage-1 image whole, so this has to cover every SPL; A1N's, the largest, is
/// 34 KB.
const STREAM_ABOVE: u32 = 64 * 1024;

/// The daemon's deepest path, a streamed write with its verify, peaks at about 35 KB.
const DAEMON_STACK: usize = 48 * 1024;

const REPORT_EVERY: Duration = Duration::from_secs(10);
/// Every sixth memory report is followed by one of every task's stack.
const STACKS_EVERY: u32 = 6;

/// The daemon thread's FreeRTOS handle, for the memory report's stack high-water mark.
static DAEMON_TASK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(core::ptr::null_mut());

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    // Once the daemon runs on its own thread this returns, and ESP-IDF deletes the main
    // task and frees its stack.
    if let Err(err) = run() {
        error!("{err}");
    }
}

fn run() -> Result<(), String> {
    // tokio's reactor (mio) wakes itself through an eventfd.
    let eventfs = MountedEventfs::mount(4).map_err(|err| format!("eventfd: {err}"))?;
    let peripherals = Peripherals::take().map_err(|err| err.to_string())?;
    let sysloop = EspSystemEventLoop::take().map_err(|err| err.to_string())?;
    #[cfg(not(esp32p4))]
    let (network, hostname, settings) = {
        let nvs = EspDefaultNvsPartition::take().map_err(|err| err.to_string())?;
        let Some(saved) = wifi::saved(&nvs)? else {
            // Nothing to join: the portal is all this boot does, until it is given a network.
            return portal::run(peripherals.modem, sysloop, nvs).map(|never| match never {});
        };
        let hostname = saved.hostname.clone();
        let (forget, store) = (nvs.clone(), nvs.clone());
        let settings = status::Wifi {
            reset: Box::new(move || {
                wifi::forget(&forget)?;
                portal::restart_soon();
                Ok(format!("Wi-Fi settings erased; restarting into the setup portal {}", wifi::portal_ssid()))
            }),
            set_tx_power: Box::new(move |power| wifi::set_tx_power(&store, power)),
            tx_power: wifi::tx_power,
            browned_out: wifi::browned_out(),
        };
        (wifi::join(peripherals.modem, sysloop, nvs, saved)?, hostname, Some(settings))
    };
    // Ethernet has nothing to set up.
    #[cfg(esp32p4)]
    let (network, hostname, settings) = {
        let hostname = eth::default_hostname();
        (eth::start(sysloop, &hostname)?, hostname, None)
    };
    // Power, boot pin, UART TX and RX. The P4's are placeholders until a board is chosen:
    // clear of its Ethernet, console and strapping pins, and nothing more is known.
    #[cfg(not(esp32p4))]
    let (power, boot, tx, rx) =
        (peripherals.pins.gpio15, peripherals.pins.gpio16, peripherals.pins.gpio17, peripherals.pins.gpio18);
    #[cfg(esp32p4)]
    let (power, boot, tx, rx) =
        (peripherals.pins.gpio20, peripherals.pins.gpio21, peripherals.pins.gpio22, peripherals.pins.gpio23);
    // The clip on the camera's flash chip for flashrom: CS, CLK, MISO and the switch on its
    // VCC. MOSI is the boot pin, on the flash's DI already. The P4's are placeholders too.
    #[cfg(not(esp32p4))]
    let (cs, clk, miso, vcc) =
        (peripherals.pins.gpio10, peripherals.pins.gpio12, peripherals.pins.gpio13, peripherals.pins.gpio14);
    #[cfg(esp32p4)]
    let (cs, clk, miso, vcc) =
        (peripherals.pins.gpio45, peripherals.pins.gpio46, peripherals.pins.gpio47, peripherals.pins.gpio48);
    let host = UsbHost::install().map_err(|err| err.to_string())?;
    let camera = camera::start(power, boot, host.clone())?;
    console::start(peripherals.uart1, tx, rx, Arc::clone(&camera))?;
    // The rest of the unit works without it.
    if let Err(err) = serprog::start(cs, clk, miso, vcc, Arc::clone(&camera)) {
        error!("{err}");
    }
    // Findable as a camera is: the app's hub lists it and opens the page on port 80.
    let status = status::start(&hostname, camera, settings)?;
    memory_report("after the network and USB host");

    // The daemon's operations are deep async state machines, and a transfer blocks the
    // runtime for its duration; one thread serves one client at a time.
    std::thread::Builder::new()
        .name("dfu-remote".into())
        .stack_size(DAEMON_STACK)
        .spawn(move || daemon(host))
        .map_err(|err| err.to_string())?;
    // These run for the life of the firmware, which the main task does not.
    core::mem::forget((eventfs, network, status));
    Ok(())
}

fn daemon(host: UsbHost) {
    DAEMON_TASK.store(unsafe { sys::xTaskGetCurrentTaskHandle() }.cast(), Ordering::Relaxed);
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => return error!("tokio runtime: {err}"),
    };
    runtime.block_on(async move {
        tokio::spawn(async {
            let mut reports = 0u32;
            loop {
                tokio::time::sleep(REPORT_EVERY).await;
                memory_report("periodic");
                reports = reports.wrapping_add(1);
                if reports.is_multiple_of(STACKS_EVERY) {
                    stacks_report();
                }
            }
        });
        let addresses = [
            SocketAddr::from((Ipv6Addr::UNSPECIFIED, DEFAULT_PORT)),
            SocketAddr::from((Ipv4Addr::UNSPECIFIED, DEFAULT_PORT)),
        ];
        let listener = match listen::bind(&addresses) {
            Ok(listener) => listener,
            Err(err) => return error!("dfu-remote: {err}"),
        };
        match listener.local_addr() {
            Ok(bound) => info!("dfu-remote listening on {bound}"),
            Err(err) => info!("dfu-remote listening (address unknown: {err})"),
        }
        let auth = Auth::open();
        // No disk to stage a READ in: the alt is read twice instead.
        let mut state = DaemonState::new(host, TokioClock, NO_FIRMWARE_DIR)
            .with_stream_above(STREAM_ABOVE)
            .with_read_staging(ReadStaging::TwoPass);
        serve(listener, &auth, Timeouts::default(), &Origins::SHIPPED, &mut state, NoSignals).await;
    });
}

/// Free and lowest-ever free heap, internal and PSRAM, and the stack high-water marks of the
/// daemon and console threads.
fn memory_report(when: &str) {
    let (internal, spiram) = (sys::MALLOC_CAP_INTERNAL, sys::MALLOC_CAP_SPIRAM);
    let free = |caps| unsafe { sys::heap_caps_get_free_size(caps) };
    let lowest = |caps| unsafe { sys::heap_caps_get_minimum_free_size(caps) };
    let dma = unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL) };
    let unused = |task: &AtomicPtr<core::ffi::c_void>| {
        let task = task.load(Ordering::Relaxed);
        if task.is_null() { 0 } else { unsafe { sys::uxTaskGetStackHighWaterMark(task.cast()) } }
    };
    info!(
        "memory {when}: internal free {} (lowest {}), DMA block {dma}, psram free {} (lowest {}), stack unused: daemon {}, console {}",
        free(internal),
        lowest(internal),
        free(spiram),
        lowest(spiram),
        unused(&DAEMON_TASK),
        unused(&console::TASK)
    );
}

/// Every task's unused stack: what each could give up. Threads spawned from Rust are all
/// named `pthread`; the daemon and the console are named from their handles.
fn stacks_report() {
    let count = unsafe { sys::uxTaskGetNumberOfTasks() };
    let mut tasks: Vec<sys::TaskStatus_t> = Vec::with_capacity(count as usize + 2);
    let filled = unsafe { sys::uxTaskGetSystemState(tasks.as_mut_ptr(), count + 2, core::ptr::null_mut()) };
    // SAFETY: uxTaskGetSystemState initialised the first `filled` entries.
    unsafe { tasks.set_len((filled as usize).min(tasks.capacity())) };
    let daemon = DAEMON_TASK.load(Ordering::Relaxed);
    let console = console::TASK.load(Ordering::Relaxed);
    let mut line = String::new();
    for task in &tasks {
        let handle: *mut core::ffi::c_void = task.xHandle.cast();
        let name = if handle == daemon {
            "dfu-remote".into()
        } else if handle == console {
            "console".into()
        } else {
            unsafe { core::ffi::CStr::from_ptr(task.pcTaskName) }.to_string_lossy()
        };
        line.push_str(&format!(" {name} {}", task.usStackHighWaterMark));
    }
    info!("stacks unused:{line}");
}

/// Spawns a thread whose FreeRTOS task is called `name`, as the stack report prints it: a
/// Rust thread's own name never reaches FreeRTOS.
fn spawn_named(
    name: &'static core::ffi::CStr,
    stack: usize,
    body: impl FnOnce() + Send + 'static,
) -> Result<(), String> {
    ThreadSpawnConfiguration {
        name: Some(name),
        ..Default::default()
    }
    .set()
    .map_err(|err| err.to_string())?;
    let spawned = std::thread::Builder::new()
        .name(name.to_string_lossy().into_owned())
        .stack_size(stack)
        .spawn(body);
    ThreadSpawnConfiguration::default().set().map_err(|err| err.to_string())?;
    spawned.map(drop).map_err(|err| err.to_string())
}

/// There is nothing to interrupt the daemon on the unit; power is the off switch.
struct NoSignals;

impl Signals for NoSignals {
    async fn next(&mut self) {
        core::future::pending::<()>().await;
    }
}
