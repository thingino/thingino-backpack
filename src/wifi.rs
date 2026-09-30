//! Joining the saved network, and staying joined.
//!
//! ESP-IDF does not rejoin by itself: after a disconnect the station stays down until
//! something calls `esp_wifi_connect` again, and the unit once stayed off the network for
//! good after a brief outage. Every loss, a failed first join included, schedules another
//! attempt, backing off from one second to thirty.

use core::time::Duration;
use std::ffi::CString;
use std::net::IpAddr;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use esp_idf_svc::eventloop::{EspSubscription, EspSystemEventLoop, System};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::netif::{EspNetif, IpEvent, NetifConfiguration, NetifStack};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::sys::{self, EspError};
use esp_idf_svc::wifi::{AuthMethod, ClientConfiguration, Configuration, EspWifi, WifiDriver, WifiEvent};
use log::{info, warn};

pub const NETIF_KEY: &core::ffi::CStr = c"WIFI_STA_DEF";

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LONGEST_RETRY: Duration = Duration::from_secs(30);

/// The station and what keeps it joined.
pub struct Station {
    _wifi: EspWifi<'static>,
    _events: [EspSubscription<'static, System>; 2],
}

/// What the event handlers hand to the `wifi` thread. They run on the system event task,
/// so they only forward.
enum Change {
    Started,
    Joined,
    Left(u16),
    Address(IpAddr),
}

/// The NVS namespace and keys usbipdcpp_esp32 uses, so a board provisioned by that firmware
/// keeps its credentials; `hostname` is this firmware's own.
const NAMESPACE: &str = "wifi";
const SSID: &str = "ssid";
const SECRET: &str = "passwd";
const HOSTNAME: &str = "hostname";

/// The network to join, as saved in NVS.
pub struct Saved {
    pub ssid: String,
    /// A passphrase, a 64-hex-digit PSK (which the driver takes as the key itself), or
    /// empty for an open network.
    pub secret: String,
    pub hostname: String,
}

/// The saved network, or `None` on a board that has never been given one.
pub fn saved(nvs: &EspDefaultNvsPartition) -> Result<Option<Saved>, String> {
    // Read-write, so a fresh board's missing namespace is created rather than an error.
    let store = EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))?;
    let read = |key: &str| -> Result<Option<String>, String> {
        let mut buf = [0u8; 100];
        Ok(store.get_str(key, &mut buf).map_err(|err| err.to_string())?.map(str::to_owned))
    };
    let Some(ssid) = read(SSID)?.filter(|ssid| !ssid.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(Saved {
        ssid,
        secret: read(SECRET)?.unwrap_or_default(),
        hostname: read(HOSTNAME)?.filter(|name| !name.is_empty()).unwrap_or_else(default_hostname),
    }))
}

/// Saves the network to join, and the hostname when one is given.
pub fn store(nvs: &EspDefaultNvsPartition, ssid: &str, secret: &str, hostname: Option<&str>) -> Result<(), String> {
    let store = EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))?;
    store.set_str(SSID, ssid).map_err(|err| err.to_string())?;
    store.set_str(SECRET, secret).map_err(|err| err.to_string())?;
    if let Some(hostname) = hostname {
        store.set_str(HOSTNAME, hostname).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// Forgets the network and the hostname, so the next boot opens the setup portal.
pub fn forget(nvs: &EspDefaultNvsPartition) -> Result<(), String> {
    let store = EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))?;
    for key in [SSID, SECRET, HOSTNAME] {
        store.remove(key).map_err(|err| err.to_string())?;
    }
    Ok(())
}

/// The name of the setup portal's access point.
pub fn portal_ssid() -> String {
    format!("THINGINO-BACKPACK-{}", unit_suffix())
}

/// The last two octets of the access point MAC, as the thingino cameras name their portal.
pub fn unit_suffix() -> String {
    let mut mac = [0u8; 6];
    unsafe { sys::esp_read_mac(mac.as_mut_ptr(), sys::esp_mac_type_t_ESP_MAC_WIFI_SOFTAP) };
    format!("{:02x}{:02x}", mac[4], mac[5])
}

pub fn default_hostname() -> String {
    format!("thingino-backpack-{}", unit_suffix())
}

/// Starts joining `saved`. Returns once the radio is up; the join completes in the
/// background.
pub fn join(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    saved: Saved,
) -> Result<Station, String> {
    let Saved { ssid, secret, hostname } = saved;
    let driver = WifiDriver::new(modem, sysloop.clone(), Some(nvs)).map_err(|err| err.to_string())?;
    let sta = EspNetif::new_with_conf(&sta_configuration()).map_err(|err| err.to_string())?;
    let ap = EspNetif::new(NetifStack::Ap).map_err(|err| err.to_string())?;
    let mut wifi = EspWifi::wrap_all(driver, sta, ap).map_err(|err| err.to_string())?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: ssid.as_str().try_into().map_err(|_| "saved SSID is too long".to_owned())?,
        password: secret.as_str().try_into().map_err(|_| "saved password is too long".to_owned())?,
        auth_method: if secret.is_empty() { AuthMethod::None } else { AuthMethod::WPA2Personal },
        ..Default::default()
    }))
    .map_err(|err| err.to_string())?;
    // DHCP sends it, so the unit is findable by name in the router's lease table.
    let name = CString::new(hostname.as_str()).map_err(|_| "saved hostname has a NUL".to_owned())?;
    unsafe { sys::esp_netif_set_hostname(wifi.sta_netif().handle(), name.as_ptr()) };

    let (changes, received) = mpsc::channel();
    let forward = changes.clone();
    let link = sysloop
        .subscribe::<WifiEvent, _>(move |event| {
            let change = match event {
                WifiEvent::StaStarted => Change::Started,
                WifiEvent::StaConnected(_) => Change::Joined,
                WifiEvent::StaDisconnected(left) => Change::Left(left.reason()),
                _ => return,
            };
            let _ = forward.send(change);
        })
        .map_err(|err| err.to_string())?;
    let addresses = sysloop
        .subscribe::<IpEvent, _>(move |event| {
            let address = match event {
                IpEvent::DhcpIpAssigned(got) => IpAddr::V4(got.ip()),
                IpEvent::DhcpIp6Assigned(got) => IpAddr::V6(got.addr()),
                _ => return,
            };
            let _ = changes.send(Change::Address(address));
        })
        .map_err(|err| err.to_string())?;

    let netif = wifi.sta_netif().handle() as usize;
    crate::spawn_named(c"rejoin", 4096, move || stay_joined(&ssid, netif, received))?;
    wifi.start().map_err(|err| err.to_string())?;
    // Every DFU block is a request/response pair; modem sleep adds 100 ms stalls to each.
    unsafe { sys::esp_wifi_set_ps(sys::wifi_ps_type_t_WIFI_PS_NONE) };
    Ok(Station {
        _wifi: wifi,
        _events: [link, addresses],
    })
}

/// esp-idf-svc's client defaults leave out two flags ESP-IDF's own default station netif
/// sets: without IPV6_AUTOCONFIG lwIP ignores every prefix a router advertises and the
/// unit keeps only its link-local address, and without MLDV6_REPORT a snooping switch may
/// stop forwarding the solicitations that reach its addresses.
fn sta_configuration() -> NetifConfiguration {
    let default = NetifConfiguration::wifi_default_client();
    NetifConfiguration {
        flags: default.flags
            | sys::esp_netif_flags_ESP_NETIF_FLAG_IPV6_AUTOCONFIG_ENABLED
            | sys::esp_netif_flags_ESP_NETIF_FLAG_MLDV6_REPORT,
        ..default
    }
}

fn stay_joined(ssid: &str, netif: usize, changes: Receiver<Change>) {
    let mut wait = FIRST_RETRY;
    let mut joined = false;
    for change in changes {
        match change {
            Change::Started => attempt(Duration::ZERO, &mut wait),
            Change::Joined => {
                info!("wifi: joined {ssid}");
                joined = true;
                wait = FIRST_RETRY;
                // The netif drops its IPv6 addresses whenever the link goes down.
                unsafe { sys::esp_netif_create_ip6_linklocal(netif as *mut sys::esp_netif_t) };
            }
            Change::Left(reason) => {
                let what = if joined { "lost" } else { "could not join" };
                warn!("wifi: {what} {ssid} (reason {reason}), trying again in {} s", wait.as_secs());
                joined = false;
                let delay = wait;
                wait = (wait * 2).min(LONGEST_RETRY);
                attempt(delay, &mut wait);
            }
            Change::Address(address) => info!("wifi: address {address}"),
        }
    }
}

/// Starts a join after `delay`. An attempt the driver starts ends in `Joined` or `Left`;
/// one it refuses raises no event at all, so that one is retried here.
fn attempt(delay: Duration, wait: &mut Duration) {
    let mut delay = delay;
    loop {
        thread::sleep(delay);
        match EspError::from(unsafe { sys::esp_wifi_connect() }) {
            None => return,
            Some(err) => warn!("wifi: the driver refused to join ({err}), trying again in {} s", wait.as_secs()),
        }
        delay = *wait;
        *wait = (*wait * 2).min(LONGEST_RETRY);
    }
}
