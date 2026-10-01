//! First-boot provisioning, the way thingino cameras do it, so the thingino app and a phone
//! browser both work unchanged.
//!
//! A board with no saved network opens an open access point `THINGINO-BACKPACK-<mac>` (the
//! app's picker matches the `THINGINO-` prefix) at 172.16.0.1 and answers the cameras' portal
//! API at `/x/api.cgi` (`package/wifi/files/api.cgi` in thingino-firmware): `get_info`, the
//! four scan actions, and `save`, which writes the network to NVS and restarts. `get_info`
//! advertises `wlan_psk` and `rootpass_hash`, so the app derives the Wi-Fi key on the phone
//! and no passphrase crosses the open access point. It also advertises `wifi_only`: there is
//! no root account, time zone, SSH key or access-point mode here, so an app that knows the
//! flag leaves those out, and the fields an older app sends anyway are accepted and ignored
//! (`rootpass_hash` stays advertised so that such an app still derives the key). A catch-all DNS makes phones raise their
//! sign-in page, which is served at `/`. The dfu-remote daemon does not run here: the access
//! point is open to anyone in range.

use core::convert::Infallible;
use core::time::Duration;
use std::net::{Ipv4Addr, UdpSocket};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::http::server::{Configuration as HttpConfiguration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write};
use esp_idf_svc::ipv4::{self, Mask, RouterConfiguration, Subnet};
use esp_idf_svc::netif::{EspNetif, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AccessPointInfo, AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver,
};
use log::{info, warn};

use crate::wifi;

const PORTAL_IP: Ipv4Addr = Ipv4Addr::new(172, 16, 0, 1);
const PAGE: &str = include_str!("portal.html");
/// How long `scan_networks` waits for a sweep it asked for; the app reads for 20 s.
const SCAN_WAIT: Duration = Duration::from_secs(12);
/// The app's form, an SSH key included, is well under this.
const LARGEST_FORM: usize = 4096;

/// Runs the portal until it is given a network, and then restarts into it.
pub fn run(modem: Modem<'static>, sysloop: EspSystemEventLoop, nvs: EspDefaultNvsPartition) -> Result<Infallible, String> {
    let driver = WifiDriver::new(modem, sysloop, Some(nvs.clone())).map_err(|err| err.to_string())?;
    let sta = EspNetif::new(NetifStack::Sta).map_err(|err| err.to_string())?;
    let ap = EspNetif::new_with_conf(&NetifConfiguration {
        ip_configuration: Some(ipv4::Configuration::Router(RouterConfiguration {
            subnet: Subnet {
                gateway: PORTAL_IP,
                mask: Mask(24),
            },
            dhcp_enabled: true,
            dns: Some(PORTAL_IP),
            secondary_dns: None,
        })),
        ..NetifConfiguration::wifi_default_router()
    })
    .map_err(|err| err.to_string())?;
    let mut wifi = EspWifi::wrap_all(driver, sta, ap).map_err(|err| err.to_string())?;

    // One sweep before the access point is up, so the first page lists networks without a
    // scan that stalls the access point under the phone that has just joined it.
    wifi.set_configuration(&Configuration::Client(ClientConfiguration::default()))
        .map_err(|err| err.to_string())?;
    wifi.start().map_err(|err| err.to_string())?;
    let first = sweep(&mut wifi);
    wifi.stop().map_err(|err| err.to_string())?;

    let ssid = wifi::portal_ssid();
    wifi.set_configuration(&Configuration::Mixed(
        ClientConfiguration::default(),
        AccessPointConfiguration {
            ssid: ssid.as_str().try_into().map_err(|_| "portal SSID is too long".to_owned())?,
            auth_method: AuthMethod::None,
            channel: 1,
            max_connections: 4,
            ..Default::default()
        },
    ))
    .map_err(|err| err.to_string())?;
    wifi.start().map_err(|err| err.to_string())?;
    info!("portal: no saved network; open access point {ssid} at {PORTAL_IP}");

    let scans = Arc::new(Scans::new(first));
    let (wanted, sweeps) = mpsc::channel();
    thread::Builder::new()
        .name("portal-dns".into())
        .stack_size(4096)
        .spawn(answer_dns)
        .map_err(|err| err.to_string())?;
    let _server = serve(&scans, &wanted, &nvs)?;

    // The radio stays with this thread: a handler asks for a sweep and waits for its result.
    for () in sweeps {
        let found = sweep(&mut wifi);
        scans.finish(found);
    }
    Err("the portal stopped taking scan requests".to_owned())
}

struct Network {
    ssid: String,
    bssid: [u8; 6],
    signal: i8,
    security: &'static str,
}

impl From<AccessPointInfo> for Network {
    fn from(ap: AccessPointInfo) -> Self {
        // The cameras report WPA2, WPA, WEP or Open; anything newer needs a key as WPA2 does.
        let security = match ap.auth_method {
            None | Some(AuthMethod::None) => "Open",
            Some(AuthMethod::WEP) => "WEP",
            Some(AuthMethod::WPA) => "WPA",
            Some(_) => "WPA2",
        };
        Self {
            ssid: ap.ssid.as_str().to_owned(),
            bssid: ap.bssid,
            signal: ap.signal_strength,
            security,
        }
    }
}

fn sweep(wifi: &mut EspWifi<'static>) -> Vec<Network> {
    match wifi.scan() {
        Ok(found) => found
            .into_iter()
            .filter(|ap| !ap.ssid.is_empty())
            .map(Network::from)
            .collect(),
        Err(err) => {
            warn!("portal: scan failed: {err}");
            Vec::new()
        }
    }
}

struct ScanState {
    networks: Vec<Network>,
    scanning: bool,
}

struct Scans {
    state: Mutex<ScanState>,
    done: Condvar,
}

impl Scans {
    fn new(networks: Vec<Network>) -> Self {
        Self {
            state: Mutex::new(ScanState {
                networks,
                scanning: false,
            }),
            done: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, ScanState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks for a sweep unless one is under way already.
    fn start(&self, wanted: &Sender<()>) {
        let mut state = self.lock();
        if !state.scanning && wanted.send(()).is_ok() {
            state.scanning = true;
        }
    }

    fn finish(&self, networks: Vec<Network>) {
        let mut state = self.lock();
        state.networks = networks;
        state.scanning = false;
        self.done.notify_all();
    }

    /// A fresh sweep's networks, or the last ones if it does not finish in time.
    fn fresh(&self, wanted: &Sender<()>) -> String {
        self.start(wanted);
        let state = self.lock();
        let (state, _) = self
            .done
            .wait_timeout_while(state, SCAN_WAIT, |state| state.scanning)
            .unwrap_or_else(PoisonError::into_inner);
        networks_json(&state.networks)
    }

    fn cached(&self) -> String {
        networks_json(&self.lock().networks)
    }

    fn status(&self) -> String {
        let state = self.lock();
        if state.scanning {
            r#"{"scanning": true}"#.to_owned()
        } else {
            networks_json(&state.networks)
        }
    }
}

fn networks_json(networks: &[Network]) -> String {
    let entries: Vec<String> = networks
        .iter()
        .map(|net| {
            format!(
                r#"{{"ssid": {}, "bssid": "{}", "signal": {}, "security": "{}"}}"#,
                json_string(&net.ssid),
                mac_text(net.bssid),
                net.signal,
                net.security
            )
        })
        .collect();
    format!(r#"{{"networks": [{}]}}"#, entries.join(", "))
}

fn info_json() -> String {
    let mut mac = [0u8; 6];
    unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_STA) };
    format!(
        r#"{{"hostname": {}, "image_id": "thingino-backpack", "build_id": {}, "wlan_mac": "{}", "features": ["wlan_psk", "rootpass_hash", "wifi_only"]}}"#,
        json_string(&wifi::default_hostname()),
        json_string(&crate::status::build_id()),
        mac_text(mac)
    )
}


fn mac_text(mac: [u8; 6]) -> String {
    mac.iter().map(|byte| format!("{byte:02x}")).collect::<Vec<_>>().join(":")
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

type Req<'a, 'r> = Request<&'a mut EspHttpConnection<'r>>;

fn serve(scans: &Arc<Scans>, wanted: &Sender<()>, nvs: &EspDefaultNvsPartition) -> Result<EspHttpServer<'static>, String> {
    let mut server = EspHttpServer::new(&HttpConfiguration {
        uri_match_wildcard: true,
        stack_size: 10 * 1024,
        ..Default::default()
    })
    .map_err(|err| err.to_string())?;

    let (scans, wanted) = (Arc::clone(scans), wanted.clone());
    server
        .fn_handler("/x/api.cgi", Method::Get, move |req: Req<'_, '_>| {
            let body = match query(req.uri(), "action").as_deref() {
                Some("get_info") => info_json(),
                Some("scan_networks") => scans.fresh(&wanted),
                Some("scan_cached") => scans.cached(),
                Some("scan_start") => {
                    scans.start(&wanted);
                    r#"{"scanning": true}"#.to_owned()
                }
                Some("scan_status") => scans.status(),
                _ => r#"{"error": "Invalid action"}"#.to_owned(),
            };
            reply_json(req, &body)
        })
        .map_err(|err| err.to_string())?;

    let nvs = nvs.clone();
    server
        .fn_handler("/x/api.cgi", Method::Post, move |mut req: Req<'_, '_>| {
            // `action` rides in the query string even on a POST, as on the cameras.
            if query(req.uri(), "action").as_deref() != Some("save") {
                return reply_json(req, r#"{"error": "Invalid action"}"#);
            }
            let form = read_form(&mut req)?;
            let body = match save(&nvs, &form) {
                Ok(()) => {
                    crate::restart_soon();
                    r#"{"success": true}"#.to_owned()
                }
                Err(error) => format!(r#"{{"success": false, "error": {}}}"#, json_string(&error)),
            };
            reply_json(req, &body)
        })
        .map_err(|err| err.to_string())?;

    server
        .fn_handler("/", Method::Get, |req: Req<'_, '_>| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])?
                .write_all(PAGE.as_bytes())
        })
        .map_err(|err| err.to_string())?;

    // Everything else, the phones' connectivity probes included, is sent to the page, which
    // is what makes a phone offer to sign in to the network.
    server
        .fn_handler("/*", Method::Get, |req: Req<'_, '_>| {
            req.into_response(302, Some("Found"), &[("Location", "http://172.16.0.1/")])
                .map(drop)
        })
        .map_err(|err| err.to_string())?;
    Ok(server)
}

fn reply_json(req: Req<'_, '_>, body: &str) -> Result<(), EspIOError> {
    req.into_response(
        200,
        None,
        &[("Content-Type", "application/json; charset=UTF-8"), ("Cache-Control", "no-store")],
    )?
    .write_all(body.as_bytes())
}

fn read_form(req: &mut Req<'_, '_>) -> Result<Vec<(String, String)>, EspIOError> {
    let mut body = Vec::new();
    let mut buf = [0u8; 512];
    while body.len() < LARGEST_FORM {
        let got = req.read(&mut buf)?;
        if got == 0 {
            break;
        }
        body.extend_from_slice(&buf[..got]);
    }
    Ok(pairs(&String::from_utf8_lossy(&body)))
}

/// Validates the form as the cameras' `save_config` does, and stores the network.
fn save(nvs: &EspDefaultNvsPartition, form: &[(String, String)]) -> Result<(), String> {
    let field = |key: &str| form.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
    if field("wlan_ap") == Some("true") {
        return Err("The backpack joins a network; it cannot be its own access point.".to_owned());
    }
    let ssid = field("wlan_ssid").unwrap_or_default();
    if ssid.is_empty() {
        return Err("Enter the network name.".to_owned());
    }
    if ssid.len() > 32 {
        return Err("A network name is at most 32 bytes.".to_owned());
    }
    let is_psk = |key: &str| key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit());
    let secret = match field("wlan_psk").filter(|psk| !psk.is_empty()) {
        Some(psk) if is_psk(psk) => psk,
        Some(_) => return Err("wlan_psk must be 64 hex characters".to_owned()),
        None => {
            let pass = field("wlan_pass").unwrap_or_default();
            if !(pass.is_empty() || (8..=63).contains(&pass.len()) || is_psk(pass)) {
                return Err("A WPA passphrase is 8 to 63 characters.".to_owned());
            }
            pass
        }
    };
    let hostname = field("hostname").map(str::trim).filter(|name| !name.is_empty());
    if let Some(name) = hostname {
        let bad: String = name
            .chars()
            .filter(|c| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '.'))
            .collect();
        if !bad.is_empty() {
            return Err(format!("Hostname cannot contain {bad}"));
        }
        if name.len() > 32 {
            return Err("A hostname is at most 32 characters.".to_owned());
        }
    }
    wifi::store(nvs, ssid, secret, hostname)?;
    info!("portal: saved {ssid}; restarting to join it");
    Ok(())
}

fn query(uri: &str, key: &str) -> Option<String> {
    let (_, query) = uri.split_once('?')?;
    pairs(query).into_iter().find(|(name, _)| name == key).map(|(_, value)| value)
}

/// An `application/x-www-form-urlencoded` body or query string, decoded.
fn pairs(text: &str) -> Vec<(String, String)> {
    text.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(name), decode(value))
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = |offset: usize| bytes.get(at + offset).and_then(|&byte| char::from(byte).to_digit(16));
        match (bytes[at], hex(1), hex(2)) {
            (b'%', Some(high), Some(low)) => {
                out.push(u8::try_from(high * 16 + low).unwrap_or_default());
                at += 3;
            }
            (b'+', _, _) => {
                out.push(b' ');
                at += 1;
            }
            (byte, _, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Answers every name with the portal's address, as the cameras' wildcard dnsd does, so a
/// phone's connectivity check lands on the page.
fn answer_dns() {
    let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 53)) {
        Ok(socket) => socket,
        Err(err) => return warn!("portal: DNS: {err}"),
    };
    let mut query = [0u8; 512];
    loop {
        match socket.recv_from(&mut query) {
            Ok((len, peer)) => {
                if let Some(reply) = dns_reply(&query[..len]) {
                    let _ = socket.send_to(&reply, peer);
                }
            }
            Err(_) => thread::sleep(Duration::from_millis(100)),
        }
    }
}

/// The reply to a one-question standard query: an A record for the portal when it asked
/// for A (or ANY), and an empty answer otherwise.
fn dns_reply(query: &[u8]) -> Option<Vec<u8>> {
    let header = query.get(..12)?;
    // A response, or anything but one question, is not ours to answer.
    if header[2] & 0x80 != 0 || header[4..6] != [0, 1] {
        return None;
    }
    let mut at = 12;
    loop {
        let len = usize::from(*query.get(at)?);
        at += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        at += len;
    }
    let qtype = u16::from_be_bytes([*query.get(at)?, *query.get(at + 1)?]);
    let end = at + 4;
    let mut reply = query.get(..end)?.to_vec();
    // QR and AA set, the client's RD kept; RA clear, NOERROR.
    reply[2] = 0x84 | (query[2] & 0x01);
    reply[3] = 0;
    let answers = u8::from(qtype == 1 || qtype == 255);
    reply[6..12].copy_from_slice(&[0, answers, 0, 0, 0, 0]);
    if answers == 1 {
        // A pointer to the question's name, type A, class IN, a one-minute TTL, 4 bytes.
        reply.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 60, 0, 4]);
        reply.extend_from_slice(&PORTAL_IP.octets());
    }
    Some(reply)
}
