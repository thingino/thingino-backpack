//! The flash programmer for browsers: `POST /api/spi`, serprog over HTTP.
//!
//! A browser cannot open a TCP socket, and an HTTPS page cannot open a WebSocket to a plain
//! `ws://` address on the LAN: Chrome's Local Network Access exempts only
//! `fetch({targetAddressSpace: 'local'})` from mixed-content blocking. So a page sends
//! serprog commands as a POST body and reads the answers from the response, streamed:
//!
//! * `O_SPIOP` (0x13): `[length out (3, LE), length in (3, LE)]`, then the bytes out;
//!   answered by ACK and the bytes in, or NAK.
//! * `O_PROGRAM` (0x30), the backpack's own: see [`crate::serprog`]; answered by ACK or NAK.
//! * `NOP` (0x00): answered by ACK.
//!
//! Anything else ends the response there. The commands go to the backpack's own serprog
//! port over loopback, so the chip is lent and given back exactly as for flashrom, and
//! flashrom, scriba and a browser exclude each other: one session at a time. The session
//! stays open between requests, so the chip is not lent again for each, and ends
//! [`RELEASE_AFTER`] after the last one, or at once with `?release=1`.
//!
//! **Which pages may use it.** Any page could otherwise read or rewrite the camera's flash,
//! so a browser's request is served only from scriba's web version, a page on the
//! operator's own machine, or the backpack's own page; a request with no `Origin`, which is
//! every non-browser client, is served too. Every answer carries the CORS headers the
//! browser needs, refusals included, as the thingino-dfu daemon does.

use core::time::Duration;
use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use esp_idf_svc::http::server::{EspHttpConnection, EspHttpServer, Request};
use esp_idf_svc::http::Method;
use esp_idf_svc::io::{EspIOError, Write as _};
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::sys::{self, EspError};
use log::{info, warn};

use crate::serprog;

/// How long the chip stays lent after a request, for the next one to find it.
const RELEASE_AFTER: Duration = Duration::from_secs(5);
/// The serprog port answers at once unless another client has it.
const SYNC_TIMEOUT: Duration = Duration::from_secs(2);
/// The chip being lent: parking a camera takes a few seconds.
const LEND_TIMEOUT: Duration = Duration::from_secs(15);
/// One command's answer: a 64 KB read at a slow clock, or a run of pages programming.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(30);

const ACK: u8 = 0x06;
const NAK: u8 = 0x15;
const NOP: u8 = 0x00;
const SYNCNOP: u8 = 0x10;
const O_SPIOP: u8 = 0x13;
const S_PIN_STATE: u8 = 0x15;
const O_PROGRAM: u8 = 0x30;

/// The origin scriba's web version is served from.
const SCRIBA: &str = "https://scriba.thingino.com";

/// The loopback serprog session, and when it last answered a request.
static LINK: Mutex<Option<(TcpStream, Instant)>> = Mutex::new(None);

fn link() -> MutexGuard<'static, Option<(TcpStream, Instant)>> {
    LINK.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn register(server: &mut EspHttpServer<'static>) -> Result<(), EspError> {
    thread::Builder::new()
        .name("spi-release".into())
        .stack_size(2048)
        .spawn(|| loop {
            thread::sleep(Duration::from_secs(1));
            let mut link = link();
            if link.as_ref().is_some_and(|(_, used)| used.elapsed() >= RELEASE_AFTER) {
                release(&mut link);
            }
        })
        .map_err(|_| EspError::from_infallible::<{ sys::ESP_ERR_NO_MEM }>())?;
    server.fn_handler("/api/spi", Method::Options, |req: Request<&mut EspHttpConnection<'_>>| {
        let headers = cors(&req);
        let mut headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        headers.extend([
            ("Access-Control-Allow-Methods", "POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "Content-Type"),
            ("Access-Control-Max-Age", "600"),
        ]);
        req.into_response(204, None, &headers)?.write_all(&[])
    })?;
    server.fn_handler("/api/spi", Method::Post, handle)?;
    Ok(())
}

/// The CORS headers for a request: its origin echoed if it may use the endpoint, and the
/// private-network grant either way. A refused origin gets no allow-origin header.
fn cors(req: &Request<&mut EspHttpConnection<'_>>) -> Vec<(&'static str, String)> {
    let mut headers = vec![("Access-Control-Allow-Private-Network", "true".to_owned()), ("Vary", "Origin".to_owned())];
    if let Some(origin) = req.header("Origin").filter(|origin| allowed(origin, req.header("Host"))) {
        headers.push(("Access-Control-Allow-Origin", origin.to_owned()));
    }
    headers
}

/// scriba's web version, a page served from this machine's loopback, or the backpack's
/// own page.
fn allowed(origin: &str, host: Option<&str>) -> bool {
    if origin == SCRIBA {
        return true;
    }
    let Some(rest) = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) else {
        return false;
    };
    let name = if rest.starts_with('[') {
        rest.split_once(']').map_or(rest, |(name, _)| name).trim_start_matches('[')
    } else {
        rest.split(':').next().unwrap_or(rest)
    };
    matches!(name, "localhost" | "127.0.0.1" | "::1") || host == Some(rest)
}

fn handle(req: Request<&mut EspHttpConnection<'_>>) -> Result<(), EspIOError> {
    let headers = cors(&req);
    let origin = req.header("Origin").map(str::to_owned);
    let release_after = req.uri().split_once('?').is_some_and(|(_, query)| query.split('&').any(|pair| pair == "release=1"));
    let mut headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let refuse = |req: Request<&mut EspHttpConnection<'_>>, status: u16, error: &str, headers: &[(&str, &str)]| {
        let mut all = headers.to_vec();
        all.push(("Content-Type", "application/json"));
        let body = format!(r#"{{"ok":false,"error":"{}"}}"#, error.replace('"', "'"));
        req.into_response(status, None, &all)?.write_all(body.as_bytes())
    };
    if origin.as_deref().is_some_and(|origin| !allowed(origin, req.header("Host"))) {
        return refuse(req, 403, "this page may not use the flash programmer", &headers);
    }
    let mut link = link();
    if link.is_none() {
        match lend() {
            Ok(stream) => *link = Some((stream, Instant::now())),
            Err(why) => return refuse(req, 409, &why, &headers),
        }
    }
    headers.push(("Content-Type", "application/octet-stream"));
    let mut resp = req.into_response(200, None, &headers)?;
    let result = {
        let (stream, used) = link.as_mut().expect("lent above");
        let result = relay(resp.connection(), stream);
        *used = Instant::now();
        result
    };
    if let Err(why) = &result {
        warn!("spi: {why}");
    }
    if result.is_err() || release_after {
        release(&mut link);
    }
    Ok(())
}

/// A loopback serprog session, synced, with the chip lent.
fn lend() -> Result<TcpStream, String> {
    // A loopback connect is immediate; Rust's connect_timeout panics on ESP-IDF's sockets.
    let at = SocketAddr::from((Ipv4Addr::LOCALHOST, serprog::PORT));
    let mut stream = TcpStream::connect(at).map_err(|err| format!("the flash programmer: {err}"))?;
    let _ = stream.set_nodelay(true);
    stream.set_read_timeout(Some(SYNC_TIMEOUT)).map_err(|err| err.to_string())?;
    let busy = |_| "the flash programmer is busy with another client (flashrom or scriba); try again once it is done".to_owned();
    stream.write_all(&[SYNCNOP]).map_err(busy)?;
    let mut last = [0_u8; 2];
    loop {
        let mut byte = [0_u8; 1];
        stream.read_exact(&mut byte).map_err(busy)?;
        last = [last[1], byte[0]];
        if last == [NAK, ACK] {
            break;
        }
    }
    stream.set_read_timeout(Some(LEND_TIMEOUT)).map_err(|err| err.to_string())?;
    stream.write_all(&[S_PIN_STATE, 1]).map_err(|err| err.to_string())?;
    let mut answer = [0_u8; 1];
    stream.read_exact(&mut answer).map_err(|err| err.to_string())?;
    if answer[0] != ACK {
        let _ = stream.shutdown(Shutdown::Both);
        let why = serprog::last_refusal().unwrap_or_else(|| "no reason given".into());
        return Err(format!("the backpack would not drive the chip: {why}"));
    }
    stream.set_read_timeout(Some(ANSWER_TIMEOUT)).map_err(|err| err.to_string())?;
    info!("spi: chip lent to a browser");
    Ok(stream)
}

fn release(link: &mut Option<(TcpStream, Instant)>) {
    if let Some((mut stream, _)) = link.take() {
        let _ = stream.write_all(&[S_PIN_STATE, 0]);
        let mut answer = [0_u8; 1];
        let _ = stream.read_exact(&mut answer);
        let _ = stream.shutdown(Shutdown::Both);
        info!("spi: chip given back");
    }
}

/// The request's commands to the session, and their answers to the response, one command
/// at a time. Ends at the end of the body.
fn relay(conn: &mut EspHttpConnection<'_>, stream: &mut TcpStream) -> Result<(), String> {
    let mut buf = vec![0_u8; 1024];
    loop {
        let mut cmd = [0_u8; 1];
        if !body_exact(conn, &mut cmd, true)? {
            return Ok(());
        }
        let head_len = match cmd[0] {
            NOP => 0,
            O_SPIOP => 6,
            O_PROGRAM => 10,
            other => return Err(format!("command {other:#04x} is not one this endpoint relays")),
        };
        let mut head = [0_u8; 10];
        body_exact(conn, &mut head[..head_len], false)?;
        // What goes out after the parameters, and what comes back after the ACK.
        let (out, read) = match cmd[0] {
            O_SPIOP => (u24(&head[..3]), u24(&head[3..6])),
            O_PROGRAM => (u24(&head[5..8]), 0),
            _ => (0, 0),
        };
        stream.write_all(&cmd).and_then(|()| stream.write_all(&head[..head_len])).map_err(|err| err.to_string())?;
        let mut left = out;
        while left > 0 {
            let n = left.min(buf.len());
            body_exact(conn, &mut buf[..n], false)?;
            stream.write_all(&buf[..n]).map_err(|err| err.to_string())?;
            left -= n;
        }
        let mut status = [0_u8; 1];
        stream.read_exact(&mut status).map_err(|err| err.to_string())?;
        conn.write_all(&status).map_err(|err| err.to_string())?;
        if status[0] != ACK {
            continue;
        }
        let mut left = read;
        while left > 0 {
            let n = left.min(buf.len());
            stream.read_exact(&mut buf[..n]).map_err(|err| err.to_string())?;
            conn.write_all(&buf[..n]).map_err(|err| err.to_string())?;
            left -= n;
        }
    }
}

/// Fills `buf` from the request body; `false` if the body ended cleanly before any of it,
/// which only `at_boundary` accepts.
///
/// Read through the raw request: esp-idf-svc refuses to read a body once the response has
/// started, and the answers stream out while the commands still come in.
fn body_exact(conn: &mut EspHttpConnection<'_>, buf: &mut [u8], at_boundary: bool) -> Result<bool, String> {
    let req = conn.raw_connection().map_err(|err| err.to_string())?.handle();
    let mut got = 0;
    let mut timeouts = 0;
    while got < buf.len() {
        let rest = &mut buf[got..];
        // SAFETY: the request lives through the handler, and `rest` is writable for its length.
        let n = unsafe { sys::httpd_req_recv(req, rest.as_mut_ptr().cast(), rest.len()) };
        if n == sys::HTTPD_SOCK_ERR_TIMEOUT && timeouts < 6 {
            timeouts += 1;
            continue;
        }
        let n = usize::try_from(n).map_err(|_| format!("request body: receive error {n}"))?;
        if n == 0 {
            return if got == 0 && at_boundary { Ok(false) } else { Err("request body ended inside a command".into()) };
        }
        got += n;
    }
    Ok(true)
}

fn u24(bytes: &[u8]) -> usize {
    usize::from(bytes[0]) | usize::from(bytes[1]) << 8 | usize::from(bytes[2]) << 16
}
