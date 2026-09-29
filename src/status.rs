//! Being findable the way a thingino camera is: an mDNS announcement of `_thingino._tcp`,
//! which the thingino app browses (its instance name is taken for the hostname, as a
//! camera's mdnsd publishes it), and a page on port 80, which the app's hub opens when the
//! entry is tapped. The page says how to reach the dfu-remote daemon and the console, and
//! drives the camera's power and boot pin through `/api/camera`:
//!
//! * `GET /api/camera`: power, boot pin, USB devices enumerated, how long a USB transfer
//!   has gone unanswered, and power cycles done for recovery, as JSON.
//! * `POST /api/camera?action=<action>`: one of [`Action::NAMES`]; answers
//!   `{"ok":true,"message":...}` or `{"ok":false,"error":...}`.

use core::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::sys;
use tdfu_daemon::DEFAULT_PORT;

use crate::camera::{self, Action, Camera};
use crate::console;

/// The announcement and the page; dropping it ends both.
pub struct Status {
    _mdns: EspMdns,
    _server: EspHttpServer<'static>,
}

pub fn start(hostname: &str, camera: Arc<Camera>) -> Result<Status, String> {
    let mut mdns = EspMdns::take().map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_hostname(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_instance_name(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.add_service(Some(hostname), "_thingino", "_tcp", 80, &[])
        .map_err(|err| format!("mDNS: {err}"))?;

    // lwIP's socket budget is shared with the daemon and mDNS, and one browser at a time
    // is all a status page sees. The handlers format onto the heap, so the task needs
    // ESP-IDF's own default stack rather than esp-idf-svc's 6 KB.
    let mut server = EspHttpServer::new(&Configuration {
        max_open_sockets: 3,
        stack_size: 4096,
        ..Default::default()
    })
    .map_err(|err| format!("status page: {err}"))?;
    let name = hostname.to_owned();
    let pins = camera.pins();
    server
        .fn_handler("/", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])?
                .write_all(page(&name, pins).as_bytes())
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
            let action = req.uri().split_once('?').and_then(|(_, query)| {
                query.split('&').find_map(|pair| pair.strip_prefix("action=")).and_then(Action::parse)
            });
            let Some(action) = action else {
                let body = format!(r#"{{"ok":false,"error":"action must be {}"}}"#, Action::NAMES);
                return req
                    .into_response(400, None, &[("Content-Type", "application/json")])?
                    .write_all(body.as_bytes());
            };
            let body = match camera.request(action) {
                Ok(message) => format!(r#"{{"ok":true,"message":"{}"}}"#, json_escape(&message)),
                Err(error) => format!(r#"{{"ok":false,"error":"{}"}}"#, json_escape(&error)),
            };
            json(req, &body)
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

/// The station's addresses as clients write them with the daemon's port: IPv6 first, as
/// the network prefers it, bracketed.
fn endpoints() -> Vec<String> {
    let netif = unsafe { sys::esp_netif_get_handle_from_ifkey(c"WIFI_STA_DEF".as_ptr()) };
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

/// `camera` is the power and boot pin GPIOs.
fn page(hostname: &str, camera: (i32, i32)) -> String {
    let name = escape(hostname);
    let endpoints = endpoints();
    // The mDNS name survives a DHCP renumbering and an ISP prefix change; the addresses
    // below it do not.
    let first = format!("{name}.local:{DEFAULT_PORT}");
    let list: String = endpoints.iter().map(|at| format!("<li><code>{}</code></li>", escape(at))).collect();
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name}</title>
<style>
body {{ font-family: system-ui, sans-serif; margin: 0 auto; max-width: 34rem; padding: 1rem; background: #111; color: #eee; }}
h1 {{ font-size: 1.3rem; margin: 0 0 .25rem; }}
.dim {{ color: #999; margin: 0 0 1rem; }}
pre, code {{ background: #1c1c1c; border-radius: .3rem; }}
pre {{ padding: .6rem; overflow-x: auto; }}
code {{ padding: 0 .2rem; }}
h2 {{ font-size: 1.1rem; margin: 1.5rem 0 .25rem; }}
table {{ border-collapse: collapse; }}
td {{ padding: .15rem .8rem .15rem 0; vertical-align: top; }}
td:first-child {{ white-space: nowrap; font-family: ui-monospace, monospace; }}
.buttons button {{ margin: 0 .25rem .5rem 0; padding: .45rem .7rem; font-size: .95rem; border-radius: .4rem; border: 1px solid #444; background: #1c1c1c; color: #eee; cursor: pointer; }}
</style>
</head>
<body>
<h1>{name}</h1>
<p class="dim">thingino backpack {version}</p>
<p>The camera on this backpack is flashed through its dfu-remote daemon. From a computer:</p>
<pre>thingino-dfu --host {first} -l</pre>
<p>In the web flasher, choose remote mode and enter <code>{first}</code>.</p>
<p>Or by address:</p>
<ul>{list}</ul>
<p>The camera's serial console is on port {console}, raw, one client at a time (a new one takes over). Ctrl-] leaves:</p>
<pre>socat -,rawer,escape=0x1d tcp:{name}.local:{console}</pre>
<p>For tools that set the baud rate or send a break, RFC 2217 on port {rfc2217}: <code>rfc2217://{name}.local:{rfc2217}</code>. Its DTR holds the boot pin and RTS cuts the power, as esptool's auto-reset drives an ESP32's IO0 and EN; both or neither asserted, as a terminal opens, leaves the camera alone.</p>
<h2>Camera</h2>
<p id="cam" class="dim">&nbsp;</p>
<p class="buttons"><button data-a="power-cycle">Power cycle</button> <button data-a="bootrom">Enter bootrom</button> <button data-a="power-off">Power off</button> <button data-a="power-on">Power on</button> <button data-a="boot-hold">Hold boot pin</button> <button data-a="boot-release">Release boot pin</button></p>
<p id="said"></p>
<h2>Wiring</h2>
<table>
<tr><td>GPIO{power}</td><td>camera power, through a MOSFET module (high: on)</td></tr>
<tr><td>GPIO{boot}</td><td>camera flash DI, pin 5 of an SOIC-8 NOR; open-drain, pulled low to boot from USB</td></tr>
<tr><td>GPIO{tx}</td><td>camera UART RX (the backpack's TX)</td></tr>
<tr><td>GPIO{rx}</td><td>camera UART TX (the backpack's RX)</td></tr>
<tr><td>GPIO19, GPIO20</td><td>camera USB D-, D+ (the ESP32-S3's OTG port)</td></tr>
<tr><td>GND</td><td>camera ground</td></tr>
</table>
{script}
</body>
</html>
"#,
        version = escape(&build_id()),
        console = console::PORT,
        rfc2217 = console::RFC2217_PORT,
        script = CAMERA_SCRIPT,
        power = camera.0,
        boot = camera.1,
        tx = console::pins().0,
        rx = console::pins().1,
    )
}

/// The camera section's buttons and its state, refreshed every few seconds.
const CAMERA_SCRIPT: &str = r"<script>
const $ = (id) => document.getElementById(id);
function show() {
  fetch('/api/camera').then((r) => r.json()).then((s) => {
    $('cam').textContent = 'Power ' + s.power + ', boot pin ' + s.boot_pin + ', ' + s.usb_devices + ' USB device(s)'
      + (s.stuck_s !== null ? ', a USB transfer unanswered for ' + s.stuck_s + ' s' : '')
      + (s.recoveries ? ', ' + s.recoveries + ' recovery power cycle(s)' : '');
  }).catch(() => { $('cam').textContent = 'The backpack did not answer.'; });
}
for (const b of document.querySelectorAll('button[data-a]')) {
  b.onclick = () => {
    $('said').textContent = b.textContent + '...';
    fetch('/api/camera?action=' + b.dataset.a, { method: 'POST' }).then((r) => r.json())
      .then((r) => { $('said').textContent = r.ok ? r.message : r.error; show(); })
      .catch(() => { $('said').textContent = 'The backpack did not answer.'; });
  };
}
show();
setInterval(show, 3000);
</script>";

fn camera_json(status: &camera::Status) -> String {
    format!(
        r#"{{"power":"{}","boot_pin":"{}","usb_devices":{},"stuck_s":{},"recoveries":{}}}"#,
        if status.powered { "on" } else { "off" },
        if status.boot_held { "held" } else { "released" },
        status.enumerated,
        status.stuck_for.map_or_else(|| "null".to_owned(), |stuck| stuck.as_secs().to_string()),
        status.recoveries,
    )
}

fn json(req: Request<&mut EspHttpConnection<'_>>, body: &str) -> Result<(), EspIOError> {
    req.into_response(200, None, &[("Content-Type", "application/json"), ("Cache-Control", "no-store")])?
        .write_all(body.as_bytes())
}

fn json_escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
