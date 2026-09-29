//! thingino-backpack: the thingino-dfu daemon (`dfu-remote`) on an ESP32-S3.
//!
//! The same tdfu-daemon library a Linux host runs, served over Wi-Fi, with the camera on
//! the OTG port driven by the ESP-IDF USB host backend. `thingino-dfu --host <backpack>`
//! and the browser flasher's remote mode talk to it unchanged.

use core::sync::atomic::{AtomicPtr, Ordering};
use core::time::Duration;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::io::vfs::MountedEventfs;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use log::{error, info};
use tdfu_daemon::auth::Auth;
use tdfu_daemon::commands::state::{DaemonState, ReadStaging};
use tdfu_daemon::serve::{serve, Signals};
use tdfu_daemon::transport::{Origins, Timeouts};
use tdfu_daemon::{listen, TokioClock, DEFAULT_PORT};

use thingino_backpack::usbhost::UsbHost;

/// Where the daemon would look for loaders. Deliberately empty: the client streams the
/// loader pair with BOOTSTRAP, so nothing is stored on the unit.
const NO_FIRMWARE_DIR: &str = "/no-loaders";

/// Request payloads over this are streamed rather than held. A streamed BOOTSTRAP still
/// holds its stage-1 image whole, so this has to cover every SPL; A1N's, the largest, is
/// 34 KB.
const STREAM_ABOVE: u32 = 64 * 1024;

/// The daemon thread's FreeRTOS handle, for the memory report's stack high-water mark.
static DAEMON_TASK: AtomicPtr<core::ffi::c_void> = AtomicPtr::new(core::ptr::null_mut());

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    if let Err(err) = run() {
        error!("{err}");
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn run() -> Result<(), String> {
    // tokio's reactor (mio) wakes itself through an eventfd.
    let _eventfs = MountedEventfs::mount(4).map_err(|err| format!("eventfd: {err}"))?;
    let peripherals = Peripherals::take().map_err(|err| err.to_string())?;
    let sysloop = EspSystemEventLoop::take().map_err(|err| err.to_string())?;
    let nvs = EspDefaultNvsPartition::take().map_err(|err| err.to_string())?;
    let _wifi = connect_wifi(peripherals.modem, sysloop, nvs)?;
    let host = UsbHost::install().map_err(|err| err.to_string())?;
    memory_report("after Wi-Fi and USB host");
    std::thread::Builder::new()
        .name("memory".into())
        .stack_size(4096)
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(10));
            memory_report("periodic");
        })
        .map_err(|err| err.to_string())?;

    // The daemon's operations are deep async state machines, and a transfer blocks the
    // runtime for its duration; one thread with a large stack serves one client at a time.
    let daemon = std::thread::Builder::new()
        .name("dfu-remote".into())
        .stack_size(64 * 1024)
        .spawn(move || daemon(host))
        .map_err(|err| err.to_string())?;
    daemon.join().map_err(|_| "the daemon thread panicked".to_owned())
}

fn daemon(host: UsbHost) {
    DAEMON_TASK.store(unsafe { sys::xTaskGetCurrentTaskHandle() }.cast(), Ordering::Relaxed);
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(err) => return error!("tokio runtime: {err}"),
    };
    runtime.block_on(async move {
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

/// Free and lowest-ever free heap, internal and PSRAM, and the daemon stack's high-water mark.
fn memory_report(when: &str) {
    let (internal, spiram) = (sys::MALLOC_CAP_INTERNAL, sys::MALLOC_CAP_SPIRAM);
    let free = |caps| unsafe { sys::heap_caps_get_free_size(caps) };
    let lowest = |caps| unsafe { sys::heap_caps_get_minimum_free_size(caps) };
    let dma = unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL) };
    let task = DAEMON_TASK.load(Ordering::Relaxed);
    let stack = if task.is_null() { 0 } else { unsafe { sys::uxTaskGetStackHighWaterMark(task.cast()) } };
    info!(
        "memory {when}: internal free {} (lowest {}), DMA block {dma}, psram free {} (lowest {}), daemon stack unused {stack}",
        free(internal),
        lowest(internal),
        free(spiram),
        lowest(spiram)
    );
}

/// There is nothing to interrupt the daemon on the unit; power is the off switch.
struct NoSignals;

impl Signals for NoSignals {
    async fn next(&mut self) {
        core::future::pending::<()>().await;
    }
}

/// Joins the network saved in NVS: the namespace and keys usbipdcpp_esp32 uses, so a board
/// provisioned by that firmware keeps its credentials.
fn connect_wifi(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
) -> Result<BlockingWifi<EspWifi<'static>>, String> {
    let (ssid, password) = saved_credentials(&nvs)?;
    let driver = EspWifi::new(modem, sysloop.clone(), Some(nvs)).map_err(|err| err.to_string())?;
    let mut wifi = BlockingWifi::wrap(driver, sysloop).map_err(|err| err.to_string())?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.as_str().try_into().map_err(|_| "saved SSID is too long".to_owned())?,
        password: password.as_str().try_into().map_err(|_| "saved password is too long".to_owned())?,
        auth_method: if password.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))
    .map_err(|err| err.to_string())?;
    wifi.start().map_err(|err| err.to_string())?;
    wifi.connect().map_err(|err| format!("joining {ssid}: {err}"))?;
    wifi.wait_netif_up().map_err(|err| err.to_string())?;
    // Every DFU block is a request/response pair; modem sleep adds 100 ms stalls to each.
    unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) };
    let netif = wifi.wifi().sta_netif();
    unsafe { sys::esp_netif_create_ip6_linklocal(netif.handle()) };
    match netif.get_ip_info() {
        Ok(ip) => info!("wifi: joined {ssid}, {}", ip.ip),
        Err(err) => info!("wifi: joined {ssid} (no IPv4 info: {err})"),
    }
    Ok(wifi)
}

fn saved_credentials(nvs: &EspDefaultNvsPartition) -> Result<(String, String), String> {
    let store = EspNvs::new(nvs.clone(), "wifi", false).map_err(|err| format!("no saved Wi-Fi: {err}"))?;
    let mut buf = [0u8; 100];
    let ssid = store
        .get_str("ssid", &mut buf)
        .map_err(|err| err.to_string())?
        .ok_or("no saved SSID")?
        .to_owned();
    let password = store.get_str("passwd", &mut buf).map_err(|err| err.to_string())?.unwrap_or("").to_owned();
    Ok((ssid, password))
}
