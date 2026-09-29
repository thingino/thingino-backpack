//! Being findable the way a thingino camera is: an mDNS announcement of `_thingino._tcp`,
//! which the thingino app browses (its instance name is taken for the hostname, as a
//! camera's mdnsd publishes it), and a page on port 80, which the app's hub opens when the
//! entry is tapped. The page says how to reach the dfu-remote daemon, which is what the
//! backpack is for.

use core::ffi::CStr;
use std::net::{Ipv4Addr, Ipv6Addr};

use esp_idf_svc::http::server::{Configuration, EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::Write;
use esp_idf_svc::mdns::EspMdns;
use esp_idf_svc::sys;
use tdfu_daemon::DEFAULT_PORT;

/// The announcement and the page; dropping it ends both.
pub struct Status {
    _mdns: EspMdns,
    _server: EspHttpServer<'static>,
}

pub fn start(hostname: &str) -> Result<Status, String> {
    let mut mdns = EspMdns::take().map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_hostname(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.set_instance_name(hostname).map_err(|err| format!("mDNS: {err}"))?;
    mdns.add_service(Some(hostname), "_thingino", "_tcp", 80, &[])
        .map_err(|err| format!("mDNS: {err}"))?;

    // lwIP's socket budget is shared with the daemon and mDNS, and one browser at a time
    // is all a status page sees. The one handler formats onto the heap, so the task needs
    // ESP-IDF's own default stack rather than esp-idf-svc's 6 KB.
    let mut server = EspHttpServer::new(&Configuration {
        max_open_sockets: 3,
        stack_size: 4096,
        ..Default::default()
    })
    .map_err(|err| format!("status page: {err}"))?;
    let name = hostname.to_owned();
    server
        .fn_handler("/", Method::Get, move |req: Request<&mut EspHttpConnection<'_>>| {
            req.into_response(200, None, &[("Content-Type", "text/html; charset=utf-8"), ("Cache-Control", "no-store")])?
                .write_all(page(&name).as_bytes())
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

fn page(hostname: &str) -> String {
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
</body>
</html>
"#,
        version = escape(&build_id()),
        console = crate::console::PORT,
    )
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
