//! Being findable the way a thingino camera is: an mDNS announcement of `_thingino._tcp`,
//! which the thingino app browses (its instance name is taken for the hostname, as a
//! camera's mdnsd publishes it), and a page on port 80, which the app's hub opens when the
//! entry is tapped. The page is static: it says how to reach the dfu-remote daemon, the
//! console and the flash programmer, draws the wiring, and drives the camera, all through
//! the API below.
//!
//! * `GET /api/info`: the unit's name, chip, firmware version, ports, addresses and the
//!   GPIOs in use, as JSON.
//! * `POST /api/spi`: serprog commands for the flash programmer, from a browser (see
//!   [`crate::spi_http`]).
//! * `POST /api/board?board=<name>`: the board the Wiring tab draws, kept on the unit;
//!   `/api/info` has it, or a guess from the chip until one is set.
//! * `GET /api/uarts`: each UART's on, name, baud rate, pins and TCP port, as JSON.
//! * `POST /api/uarts?uart=<0 to 2>&on=0|1`, `&name=<name>` or `&baud=<rate>`: turns UART 2 or 3
//!   on or off (a restart), names a UART, or sets its baud rate, now and on every boot after.
//! * `GET /api/camera`: power, boot pin, USB devices enumerated and their VID:PID, how
//!   long a USB transfer has gone unanswered, power cycles done for recovery, and BOOTSEL's
//!   sense, as JSON.
//! * `POST /api/camera?action=<action>`: one of [`Action::NAMES`]; answers
//!   `{"ok":true,"message":...}` or `{"ok":false,"error":...}`.
//! * `POST /api/camera?bootsel=low` or `high`: what BOOTSEL does to boot from USB, now and
//!   on every boot after. Same answers.
//! * `POST /api/camera?usb_port=on` or `off`: whether the backpack holds the camera's USB
//!   lines, now and on every boot after (see [`crate::usb`]). Same answers.
//! * `GET /api/flash`: whether the flash programmer drives the chip's VCC and HOLD, as JSON.
//! * `POST /api/flash?vcc=driven|alone&hold=driven|alone`, either or both: what it drives
//!   from the next flashrom session on, and on every boot after. Same answers.
//! * `GET /api/wifi`, on the Wi-Fi builds: the radio's TX power cap in dBm, whether this boot
//!   followed a brownout, which holds it at 13 dBm until the next reset, and the network's
//!   signal strength, SSID and channel, as JSON.
//! * `POST /api/wifi?tx_dbm=<dBm>`, on the Wi-Fi builds: caps the TX power, 2 to 20 dBm, now
//!   and on every boot after. Same answers as the camera's.
//! * `POST /api/wifi-reset`, on the Wi-Fi builds: forgets the network, the hostname and the
//!   TX power, and restarts into the setup portal. Same answers.
//! * `GET /api/ota`: the running firmware's version, its OTA slot and whether it is confirmed.
//! * `POST /api/ota`, a release's app image as the body: writes it to the slot not running
//!   and restarts into it (see [`crate::ota`]). Same answers.

use core::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::sys;
use tdfu_daemon::DEFAULT_PORT;

use crate::camera::{self, Action, Camera};
use crate::console;
use crate::ota;
use crate::serprog;

/// What the page changes on the Wi-Fi builds. The closures answer what to tell the user.
pub struct Wifi {
    /// Forgets the network, the hostname and the TX power, and restarts into the setup
    /// portal.
    pub reset: Box<dyn Fn() -> Result<String, String> + Send + Sync + 'static>,
    /// Caps the radio's TX power, in quarter-dBm, now and on every boot after.
    pub set_tx_power: Box<dyn Fn(i8) -> Result<String, String> + Send + Sync + 'static>,
    /// The radio's TX power cap now, in quarter-dBm.
    pub tx_power: fn() -> Option<i8>,
    /// This boot followed a brownout, which holds the radio at 13 dBm until the next reset.
    pub browned_out: bool,
    /// The network joined: signal strength in dBm, SSID and channel.
    pub link: fn() -> Option<(i8, String, u8)>,
}

/// The announcement and the page; dropping it ends both.
pub struct Status {
    _mdns: EspMdns,
    _server: EspHttpServer<'static>,
}

pub fn start(hostname: &str, camera: Arc<Camera>, wifi: Option<Wifi>, nvs: EspDefaultNvsPartition) -> Result<Status, String> {
    let _ = NVS.set(nvs);
    let mut mdns = EspMdns::take().map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_hostname(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_instance_name(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.add_service(Some(hostname), "_thingino", "_tcp", 80, &[])
        .map_err(|err| format!("mDNS: {err}"))?;

    // lwIP's socket budget is shared with the daemon and mDNS, and one browser at a time
    // is all a status page sees; but a browser opens several connections at once, and the
    // server closes the least recently used one when it runs out, which would be a quiet
    // terminal's. The handlers format onto the heap; a firmware update's write and verify
    // left 0.7 KB of 4 KB unused, but /api/spi, a loopback serprog session inside a
    // handler, overflowed 5 KB, so 8 KB.
    let mut server = EspHttpServer::new(&Configuration {
        max_open_sockets: 7,
        stack_size: 8192,
        ..Default::default()
    })
    .map_err(|err| format!("status page: {err}"))?;
    let name = hostname.to_owned();
    let pins = camera.pins();
    let has_wifi = wifi.is_some();
    server
        .fn_handler("/", Method::Get, |req: Request<&mut EspHttpConnection<'_>>| {
            let headers = [("Content-Type", "text/html; charset=utf-8"), ("Content-Encoding", "gzip"), ("Cache-Control", "no-store")];
            req.into_response(200, None, &headers)?.write_all(PAGE)
        })
        .map_err(|err| format!("status page: {err}"))?;
    for (path, font) in FONTS {
        server
            .fn_handler(path, Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
                req.into_response(200, None, &[("Content-Type", "font/woff2"), ("Cache-Control", "max-age=31536000, immutable")])?
                    .write_all(font)
            })
            .map_err(|err| format!("status page: {err}"))?;
    }
    crate::terminal::register(&mut server).map_err(|err| format!("status page: {err}"))?;
    crate::spi_http::register(&mut server).map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/board", Method::Post, |req: Request<&mut EspHttpConnection<'_>>| {
            let query = req.uri().split_once('?').map_or("", |(_, query)| query);
            let board = crate::pairs(query).into_iter().find(|(key, _)| key == "board").map(|(_, board)| board);
            match board.ok_or_else(|| "give board=<name>".to_owned()).and_then(|board| set_board(&board)) {
                Ok(message) => json(req, &answer(Ok(message))),
                Err(error) => req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(answer(Err(error)).as_bytes()),
            }
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/info", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
            json(req, &info_json(&name, pins, has_wifi))
        })
        .map_err(|err| format!("status page: {err}"))?;
    if let Some(Wifi {
        reset,
        set_tx_power,
        tx_power,
        browned_out,
        link,
    }) = wifi
    {
        server
            .fn_handler("/api/wifi-reset", Method::Post, move |req: Request<&mut EspHttpConnection<'_>>| {
                json(req, &answer(reset()))
            })
            .map_err(|err| format!("status page: {err}"))?;
        server
            .fn_handler("/api/wifi", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
                let power = tx_power().map_or_else(|| "null".to_owned(), |power| (f32::from(power) / 4.0).to_string());
                let link = link().map_or_else(
                    || r#""rssi":null,"ssid":null,"channel":null"#.to_owned(),
                    |(rssi, ssid, channel)| format!(r#""rssi":{rssi},"ssid":"{}","channel":{channel}"#, json_escape(&ssid)),
                );
                json(req, &format!(r#"{{"tx_dbm":{power},"brownout":{browned_out},{link}}}"#))
            })
            .map_err(|err| format!("status page: {err}"))?;
        server
            .fn_handler("/api/wifi", Method::Post, move |req: Request<&mut EspHttpConnection<'_>>| {
                let power = req.uri().split_once('?').and_then(|(_, query)| {
                    query.split('&').find_map(|pair| pair.strip_prefix("tx_dbm=")).and_then(quarter_dbm)
                });
                let Some(power) = power else {
                    let body = r#"{"ok":false,"error":"tx_dbm must be the TX power in dBm, 2 to 20"}"#;
                    return req
                        .into_response(400, None, &[("Content-Type", "application/json")])?
                        .write_all(body.as_bytes());
                };
                json(req, &answer(set_tx_power(power)))
            })
            .map_err(|err| format!("status page: {err}"))?;
    }
    server
        .fn_handler("/api/ota", Method::Get, |req: Request<&mut EspHttpConnection<'_>>| {
            let running = ota::running();
            let body = format!(
                r#"{{"version":"{}","slot":"{}","state":"{}"}}"#,
                json_escape(&build_id()),
                json_escape(&running.slot),
                running.state.name()
            );
            json(req, &body)
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/ota", Method::Post, |mut req: Request<&mut EspHttpConnection<'_>>| {
            let len = req.connection().header("Content-Length").and_then(|len| len.trim().parse::<usize>().ok());
            let len = len.filter(|&len| len > 0);
            let Some(len) = len else {
                let body = r#"{"ok":false,"error":"send the image as the body, with its length"}"#;
                return req
                    .into_response(411, None, &[("Content-Type", "application/json")])?
                    .write_all(body.as_bytes());
            };
            let result = ota::update(&mut req, len);
            json(req, &answer(result))
        })
        .map_err(|err| format!("status page: {err}"))?;
    let watched = Arc::clone(&camera);
    server
        .fn_handler("/api/camera", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
            json(req, &camera_json(&watched.status()))
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/camera", Method::Post, move |req: Request<&mut EspHttpConnection<'_>>| {
            let query = req.uri().split_once('?').map_or("", |(_, query)| query);
            let field = |name: &str| query.split('&').find_map(|pair| pair.strip_prefix(name));
            if let Some(port) = field("usb_port=") {
                let result = match port {
                    "on" => camera.set_usb(true),
                    "off" => camera.set_usb(false),
                    _ => Err("usb_port must be on or off".into()),
                };
                return json(req, &answer(result));
            }
            if let Some(level) = field("bootsel=") {
                let result = match level {
                    "low" => camera.set_bootsel(false),
                    "high" => camera.set_bootsel(true),
                    _ => Err("bootsel must be low or high".into()),
                };
                return json(req, &answer(result));
            }
            let Some(action) = field("action=").and_then(Action::parse) else {
                let body = format!(r#"{{"ok":false,"error":"action must be {}"}}"#, Action::NAMES);
                return req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(body.as_bytes());
            };
            json(req, &answer(camera.request(action)))
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/uarts", Method::Get, |req: Request<&mut EspHttpConnection<'_>>| {
            json(req, &console::json())
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/uarts", Method::Post, |req: Request<&mut EspHttpConnection<'_>>| {
            let query = req.uri().split_once('?').map_or("", |(_, query)| query);
            let fields = crate::pairs(query);
            let field = |name: &str| fields.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str());
            let line = field("uart").and_then(|line| line.parse::<usize>().ok());
            let result = match (line, field("on"), field("name"), field("baud")) {
                (Some(line), Some(on), _, _) => console::set_on(line, on == "1"),
                (Some(line), None, Some(name), _) => console::set_name(line, name),
                (Some(line), None, None, Some(baud)) => match baud.parse() {
                    Ok(baud) => console::set_baud(line, baud),
                    Err(_) => Err("baud must be a number".into()),
                },
                _ => Err("give uart=<0 to 2> and on=0|1, name=<name> or baud=<rate>".into()),
            };
            match result {
                Ok(message) => json(req, &answer(Ok(message))),
                Err(error) => req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(answer(Err(error)).as_bytes()),
            }
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/flash", Method::Get, |req: Request<&mut EspHttpConnection<'_>>| {
            json(req, &flash_json(serprog::driven()))
        })
        .map_err(|err| format!("status page: {err}"))?;
    server
        .fn_handler("/api/flash", Method::Post, |req: Request<&mut EspHttpConnection<'_>>| {
            let query = req.uri().split_once('?').map_or("", |(_, query)| query);
            let field = |name: &str| query.split('&').find_map(|pair| pair.strip_prefix(name));
            let setting = |name: &str| match field(name) {
                None => Some(None),
                Some("driven") => Some(Some(true)),
                Some("alone") => Some(Some(false)),
                Some(_) => None,
            };
            let (Some(vcc), Some(hold)) = (setting("vcc="), setting("hold=")) else {
                let body = r#"{"ok":false,"error":"vcc and hold must be driven or alone"}"#;
                return req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(body.as_bytes());
            };
            if vcc.is_none() && hold.is_none() {
                let body = r#"{"ok":false,"error":"give vcc, hold or both: driven or alone"}"#;
                return req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(body.as_bytes());
            }
            json(req, &answer(serprog::set_driven(vcc, hold)))
        })
        .map_err(|err| format!("status page: {err}"))?;
    Ok(Status {
        _mdns: mdns,
        _server: server,
    })
}

/// The firmware version ESP-IDF stamped into the image: the project's `git describe`.
pub fn build_id() -> String {
    let desc = unsafe { &*sys::esp_app_get_description() };
    unsafe { CStr::from_ptr(desc.version.as_ptr()) }.to_string_lossy().into_owned()
}

/// The unit's addresses as clients write them with the daemon's port: IPv6 first, as the
/// network prefers it, bracketed.
pub(crate) fn endpoints() -> Vec<String> {
    let netif = unsafe { sys::esp_netif_get_handle_from_ifkey(crate::NETIF_KEY.as_ptr()) };
    if netif.is_null() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut ip6: [sys::esp_ip6_addr_t; 4] = unsafe { core::mem::zeroed() };
    let count = unsafe { sys::esp_netif_get_all_ip6(netif, ip6.as_mut_ptr()) };
    for addr in &ip6[..usize::try_from(count).unwrap_or(0).min(ip6.len())] {
        let mut octets = [0u8; 16];
        for (chunk, word) in octets.chunks_exact_mut(4).zip(addr.addr) {
            chunk.copy_from_slice(&word.to_ne_bytes());
        }
        let ip = Ipv6Addr::from(octets);
        // A link-local address needs a zone the page cannot know.
        if !ip.is_unicast_link_local() {
            out.push(format!("[{ip}]:{DEFAULT_PORT}"));
        }
    }
    let mut ip4: sys::esp_netif_ip_info_t = unsafe { core::mem::zeroed() };
    if unsafe { sys::esp_netif_get_ip_info(netif, &mut ip4) } == 0 && ip4.ip.addr != 0 {
        out.push(format!("{}:{DEFAULT_PORT}", Ipv4Addr::from(ip4.ip.addr.to_ne_bytes())));
    }
    out
}

/// The page, `status.html` gzipped by the build: static, as everything that varies comes
/// from `/api/info` and the others.
const PAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/status.html.gz"));

/// The thingino apps' typeface, the two weights the page uses: Montserrat, latin subset, under
/// the SIL Open Font License (`fonts/OFL.txt`).
const FONTS: [(&str, &[u8]); 2] = [
    ("/montserrat-400.woff2", include_bytes!("fonts/montserrat-400.woff2")),
    ("/montserrat-600.woff2", include_bytes!("fonts/montserrat-600.woff2")),
];

/// The unit's facts for the page: its names, ports and addresses, and the GPIOs in use.
/// `camera` is the power, boot pin and BOOTSEL GPIOs.
fn info_json(hostname: &str, camera: (i32, i32, i32), wifi: bool) -> String {
    let (tx, rx) = console::pins();
    let endpoints = endpoints().iter().map(|at| format!(r#""{}""#, json_escape(at))).collect::<Vec<_>>().join(",");
    let clip = serprog::pins().map_or_else(
        || "null".to_owned(),
        |clip| {
            format!(
                r#"{{"cs":{},"miso":{},"clk":{},"mosi":{},"vcc":{},"hold":{}}}"#,
                clip.cs, clip.miso, clip.clk, clip.mosi, clip.vcc, clip.hold
            )
        },
    );
    format!(
        r#"{{"name":"{}","chip":"{}","board":"{}","version":"{}","reset":"{}","panic":{},"wifi":{wifi},"ports":{{"dfu":{DEFAULT_PORT},"console":{},"rfc2217":{},"serprog":{}}},"endpoints":[{endpoints}],"pins":{{"power":{},"boot":{},"bootsel":{},"tx":{tx},"rx":{rx},{USB_PINS},"clip":{clip}}}}}"#,
        json_escape(hostname),
        json_escape(&chip()),
        json_escape(&board()),
        json_escape(&build_id()),
        reset_reason(),
        crate::last_panic().map_or_else(|| "null".to_owned(), |text| format!(r#""{}""#, json_escape(&text))),
        console::PORT,
        console::RFC2217_PORT,
        serprog::PORT,
        camera.0,
        camera.1,
        camera.2,
    )
}

/// Where the board setting is kept.
const NAMESPACE: &str = "status";
const BOARD: &str = "board";
static NVS: std::sync::OnceLock<EspDefaultNvsPartition> = std::sync::OnceLock::new();

/// The board the Wiring tab draws: the one set on this unit, or else a guess from the chip.
fn board() -> String {
    let mut buf = [0_u8; 32];
    NVS.get()
        .and_then(|nvs| EspNvs::new(nvs.clone(), NAMESPACE, true).ok())
        .and_then(|store| store.get_str(BOARD, &mut buf).ok().flatten().map(str::to_owned))
        .unwrap_or_else(|| guessed_board().to_owned())
}

/// The page's name for one of its board drawings; it falls back to a table for one it
/// does not know.
fn set_board(board: &str) -> Result<String, String> {
    if board.is_empty() || board.len() > 24 || !board.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return Err("a board is 1 to 24 letters, digits, - or _".into());
    }
    let nvs = NVS.get().ok_or("no NVS")?;
    let store = EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))?;
    store.set_str(BOARD, board).map_err(|err| err.to_string())?;
    Ok(format!("board set to {board}"))
}

/// An S3 with its flash inside the package (the S3 Super Mini's ESP32-S3FH4R2) is a Super
/// Mini; one with it outside (a module, as on the DevKitC-1, or a XIAO) is a DevKitC-1.
#[cfg(esp32s3)]
fn guessed_board() -> &'static str {
    let mut capacity = 0_u8;
    // SAFETY: reads a 3-bit eFuse field into a byte.
    let read = unsafe {
        sys::esp_efuse_read_field_blob(core::ptr::addr_of_mut!(sys::ESP_EFUSE_FLASH_CAP).cast(), (&raw mut capacity).cast(), 3)
    };
    if read == sys::ESP_OK && capacity != 0 { "supermini" } else { "devkitc" }
}
/// The S2 Mini is the S2 board the page draws.
#[cfg(esp32s2)]
fn guessed_board() -> &'static str {
    "s2mini"
}
#[cfg(esp32p4)]
fn guessed_board() -> &'static str {
    "other"
}

/// Why the unit last started: `power-on`, or a crash, a watchdog or a brownout.
fn reset_reason() -> &'static str {
    // SAFETY: reads what the startup code recorded.
    match unsafe { sys::esp_reset_reason() } {
        sys::esp_reset_reason_t_ESP_RST_POWERON => "power-on",
        sys::esp_reset_reason_t_ESP_RST_SW => "restart",
        sys::esp_reset_reason_t_ESP_RST_PANIC => "panic",
        sys::esp_reset_reason_t_ESP_RST_INT_WDT => "interrupt watchdog",
        sys::esp_reset_reason_t_ESP_RST_TASK_WDT => "task watchdog",
        sys::esp_reset_reason_t_ESP_RST_WDT => "watchdog",
        sys::esp_reset_reason_t_ESP_RST_BROWNOUT => "brownout",
        sys::esp_reset_reason_t_ESP_RST_DEEPSLEEP => "deep sleep",
        _ => "other",
    }
}

/// Where the camera's USB lands: GPIO pins on the full-speed chips, a port of its own on the
/// P4's high-speed controller.
#[cfg(not(esp32p4))]
const USB_PINS: &str = r#""usb_dm":19,"usb_dp":20,"usb":"GPIO19, GPIO20""#;
#[cfg(esp32p4)]
const USB_PINS: &str = r#""usb":"USB 2.0 HS""#;

/// The chip ESP-IDF was built for, `esp32s3` and the like.
fn chip() -> String {
    CStr::from_bytes_until_nul(sys::CONFIG_IDF_TARGET)
        .map_or_else(|_| "an ESP32".into(), |chip| chip.to_string_lossy().into_owned())
}

fn camera_json(status: &camera::Status) -> String {
    format!(
        r#"{{"power":"{}","boot_pin":"{}","usb_devices":{},"usb":[{}],"stuck_s":{},"recoveries":{},"flash_lent":{},"bootsel":"{}","usb_port":"{}"}}"#,
        if status.powered { "on" } else { "off" },
        if status.boot_held { "held" } else { "released" },
        status.enumerated,
        status.usb_ids.iter().map(|(vid, pid)| format!(r#""{vid:04x}:{pid:04x}""#)).collect::<Vec<_>>().join(","),
        status.stuck_for.map_or_else(|| "null".to_owned(), |stuck| stuck.as_secs().to_string()),
        status.recoveries,
        status.flash_lent,
        if status.bootsel_high { "high" } else { "low" },
        if status.usb_on { "on" } else { "off" },
    )
}

fn flash_json(driven: serprog::Driven) -> String {
    let name = |driven: bool| if driven { "driven" } else { "alone" };
    format!(r#"{{"vcc":"{}","hold":"{}"}}"#, name(driven.vcc), name(driven.hold))
}

/// dBm as quarter-dBm, rounded: 16.5 is 66.
fn quarter_dbm(text: &str) -> Option<i8> {
    let quarters = (text.parse::<f32>().ok()? * 4.0).round();
    // Only a value in range converts: NaN is in no range, and `as` would saturate.
    (f32::from(i8::MIN)..=f32::from(i8::MAX)).contains(&quarters).then_some(quarters as i8)
}

/// An action's result as the API answers it.
fn answer(result: Result<String, String>) -> String {
    match result {
        Ok(message) => format!(r#"{{"ok":true,"message":"{}"}}"#, json_escape(&message)),
        Err(error) => format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(&error)),
    }
}

fn json(req: Request<&mut EspHttpConnection<'_>>, body: &str) -> Result<(), EspIOError> {
    req.into_response(200, None, &[("Content-Type", "application/json"), ("Cache-Control", "no-store")])?
        .write_all(body.as_bytes())
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out
}
