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
//! * `GET /api/wifi`, on the Wi-Fi builds: the radio's TX power cap in dBm, and whether this
//!   boot followed a brownout, which holds it at 13 dBm until the next reset, as JSON.
//! * `POST /api/wifi?tx_dbm=<dBm>`, on the Wi-Fi builds: caps the TX power, 2 to 20 dBm, now
//!   and on every boot after. Same answers as the camera's.
//! * `POST /api/wifi-reset`, on the Wi-Fi builds: forgets the network, the hostname and the
//!   TX power, and restarts into the setup portal. Same answers.

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
}

/// The announcement and the page; dropping it ends both.
pub struct Status {
    _mdns: EspMdns,
    _server: EspHttpServer<'static>,
}

pub fn start(hostname: &str, camera: Arc<Camera>, wifi: Option<Wifi>) -> Result<Status, String> {
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
    let has_wifi = wifi.is_some();
    server
        .fn_handler("/", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])?
                .write_all(page(&name, pins, has_wifi).as_bytes())
        })
        .map_err(|err| format!("status page: {err}"))?;
    if let Some(Wifi {
        reset,
        set_tx_power,
        tx_power,
        browned_out,
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
                json(req, &format!(r#"{{"tx_dbm":{power},"brownout":{browned_out}}}"#))
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
            json(req, &answer(camera.request(action)))
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
fn endpoints() -> Vec<String> {
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

/// `camera` is the power and boot pin GPIOs; `wifi`, whether the unit is on Wi-Fi, with
/// settings of its own.
fn page(hostname: &str, camera: (i32, i32), wifi: bool) -> String {
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
.buttons button, .buttons select {{ margin: 0 .25rem .5rem 0; padding: .45rem .7rem; font-size: .95rem; border-radius: .4rem; border: 1px solid #444; background: #1c1c1c; color: #eee; cursor: pointer; }}
</style>
</head>
<body>
<h1>{name}</h1>
<p class="dim">thingino backpack {version} on {chip}</p>
<p>The camera on this backpack is flashed through its dfu-remote daemon. From a computer:</p>
<pre>thingino-dfu --host {first} -l</pre>
<p>In the web flasher, choose remote mode and enter <code>{first}</code>.</p>
<p>Or by address:</p>
<ul>{list}</ul>
<p>The camera's serial console is on port {console}, raw, one client at a time (a new one takes over). Ctrl-] leaves:</p>
<pre>socat -,rawer,escape=0x1d tcp:{name}.local:{console}</pre>
<p>For tools that set the baud rate or send a break, RFC 2217 on port {rfc2217}: <code>rfc2217://{name}.local:{rfc2217}</code>. Its DTR holds the boot pin and RTS cuts the power, as esptool's auto-reset drives an ESP32's IO0 and EN; both or neither asserted, as a terminal opens, leaves the camera alone.</p>
<p>The camera's flash chip, through a SOIC-8 clip, for flashrom 1.4.0 or later on port {serprog}, with the camera off:</p>
<pre>flashrom -p serprog:ip={name}.local:{serprog} -r dump.bin</pre>
<h2>Camera</h2>
<p id="cam" class="dim">&nbsp;</p>
<p class="buttons"><button data-a="power-cycle" title="Cut the camera's power for a second, then turn it back on">Power cycle</button> <button data-a="bootrom" title="Hold the boot pin through a power cycle, and let go once the bootrom shows up on USB">Enter bootrom</button> <button data-a="power-off" title="Cut the camera's power">Power off</button> <button data-a="power-on" title="Turn the camera's power on">Power on</button> <button data-a="boot-hold" title="Pull the camera's flash DI low, so its next power-on boots from USB">Hold boot pin</button> <button data-a="boot-release" title="Let go of the boot pin">Release boot pin</button></p>
<p class="dim">Enter bootrom holds the boot pin through a power cycle, so the camera boots from USB, and lets go of the pin once the bootrom enumerates: then <code>thingino-dfu -b</code> brings up the DFU gadget.</p>
<p id="said"></p>
<h2>Wiring</h2>
<table>
<tr><td>GPIO{power}</td><td>camera power, through a MOSFET module (high: on)</td></tr>
<tr><td>GPIO{boot}</td><td>camera flash DI, pin 5 of an SOIC-8 NOR; open-drain, pulled low to boot from USB, and the flash programmer's MOSI</td></tr>
<tr><td>GPIO{tx}</td><td>camera UART RX (the backpack's TX)</td></tr>
<tr><td>GPIO{rx}</td><td>camera UART TX (the backpack's RX)</td></tr>
<tr><td>{usb}</td><td>camera USB ({usb_port})</td></tr>
<tr><td>GND</td><td>camera ground</td></tr>
{clip}
</table>
{wifi}
{script}
</body>
</html>
"#,
        version = escape(&build_id()),
        chip = chip(),
        usb = USB_PINS.0,
        usb_port = USB_PINS.1,
        console = console::PORT,
        rfc2217 = console::RFC2217_PORT,
        script = CAMERA_SCRIPT,
        wifi = if wifi { WIFI_SECTION } else { "" },
        power = camera.0,
        boot = camera.1,
        tx = console::pins().0,
        rx = console::pins().1,
        serprog = serprog::PORT,
        clip = serprog::pins().map_or_else(String::new, |clip| format!(
            "<tr><td>GPIO{}</td><td>flash CS, pin 1, through the clip</td></tr>\n\
             <tr><td>GPIO{}</td><td>flash DO, pin 2</td></tr>\n\
             <tr><td>GPIO{}</td><td>flash CLK, pin 6</td></tr>\n\
             <tr><td>GPIO{}</td><td>the switch on the clip's VCC, to pin 8 (high: on)</td></tr>",
            clip.cs, clip.miso, clip.clk, clip.vcc
        )),
    )
}

/// Where the camera's USB lands: GPIO pins on the full-speed chips, a port of its own on the
/// P4's high-speed controller.
#[cfg(not(esp32p4))]
const USB_PINS: (&str, &str) = ("GPIO19, GPIO20", "D-, D+ of the OTG port");
#[cfg(esp32p4)]
const USB_PINS: (&str, &str) = ("USB 2.0 HS", "the high-speed OTG port");

/// The chip ESP-IDF was built for, `esp32s3` and the like.
fn chip() -> String {
    CStr::from_bytes_until_nul(sys::CONFIG_IDF_TARGET)
        .map_or_else(|_| "an ESP32".into(), |chip| chip.to_string_lossy().into_owned())
}

/// The Wi-Fi builds' settings: the radio's TX power, and going back to the setup portal.
const WIFI_SECTION: &str = r#"<h2>Wi-Fi</h2>
<p class="buttons">TX power <select id="tx-power" title="The most the radio transmits at">
<option value="20">20 dBm, full</option><option value="19">19 dBm</option><option value="18">18 dBm</option><option value="17">17 dBm</option><option value="16">16 dBm</option><option value="15">15 dBm</option><option value="14">14 dBm</option><option value="13">13 dBm</option><option value="12">12 dBm</option><option value="11">11 dBm</option><option value="10">10 dBm</option><option value="9">9 dBm</option><option value="8">8 dBm</option><option value="7">7 dBm</option><option value="6">6 dBm</option><option value="5">5 dBm</option><option value="4">4 dBm</option><option value="3">3 dBm</option><option value="2">2 dBm</option>
</select> <button id="tx-set" title="Set the TX power now and on every boot after">Set</button></p>
<p class="dim">Lower it for a supply that browns out when the radio transmits, or for flash reads through a clip that come back wrong.</p>
<p id="tx-said"></p>
<p>Forget this network, the hostname and the TX power, and restart into the setup portal, as on first boot.</p>
<p class="buttons"><button id="wifi-reset" title="Forget the network, the hostname and the TX power, and restart into the setup portal">Reset Wi-Fi</button></p>
<p id="reset-said"></p>
<script>
{
  const tx = document.getElementById('tx-power');
  const said = document.getElementById('tx-said');
  fetch('/api/wifi').then((r) => r.json()).then((w) => {
    if (w.tx_dbm !== null) tx.value = String(w.tx_dbm);
    if (w.brownout) said.textContent = 'This boot followed a brownout, so the radio stays at 13 dBm or less until the next reset.';
  }).catch(() => {});
  document.getElementById('tx-set').onclick = () => {
    said.textContent = 'Setting the TX power...';
    fetch('/api/wifi?tx_dbm=' + tx.value, { method: 'POST' }).then((r) => r.json())
      .then((r) => { said.textContent = r.ok ? r.message : r.error; })
      .catch(() => { said.textContent = 'The backpack did not answer.'; });
  };
}
document.getElementById('wifi-reset').onclick = () => {
  if (!confirm('Forget the Wi-Fi settings and restart into the setup portal? The backpack leaves this network.')) return;
  const said = document.getElementById('reset-said');
  fetch('/api/wifi-reset', { method: 'POST' }).then((r) => r.json())
    .then((r) => { said.textContent = r.ok ? r.message : r.error; })
    .catch(() => { said.textContent = 'The backpack did not answer.'; });
};
</script>"#;

/// The camera section's buttons and its state, refreshed every few seconds.
const CAMERA_SCRIPT: &str = r"<script>
const $ = (id) => document.getElementById(id);
function show() {
  fetch('/api/camera').then((r) => r.json()).then((s) => {
    $('cam').textContent = 'Power ' + s.power + ', boot pin ' + s.boot_pin + ', ' + s.usb_devices + ' USB device(s)'
      + (s.stuck_s !== null ? ', a USB transfer unanswered for ' + s.stuck_s + ' s' : '')
      + (s.recoveries ? ', ' + s.recoveries + ' recovery power cycle(s)' : '')
      + (s.flash_lent ? ', flash chip lent to flashrom' : '');
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
        r#"{{"power":"{}","boot_pin":"{}","usb_devices":{},"stuck_s":{},"recoveries":{},"flash_lent":{}}}"#,
        if status.powered { "on" } else { "off" },
        if status.boot_held { "held" } else { "released" },
        status.enumerated,
        status.stuck_for.map_or_else(|| "null".to_owned(), |stuck| stuck.as_secs().to_string()),
        status.recoveries,
        status.flash_lent,
    )
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
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
