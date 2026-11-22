//! The status page's terminal: the camera's console over a WebSocket at `/api/console`.
//!
//! Each session is an ordinary console client, a loopback connection to the raw console
//! port, so it takes the console over from socat and is taken over by it the same way. The
//! camera's output goes to the page as binary frames, and what the page sends goes to the
//! camera. When the console connection ends, the page gets a text frame saying why.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpStream};
use std::sync::{Mutex, PoisonError};
use std::thread;

use esp_idf_svc::http::server::ws::EspHttpWsConnection;
use esp_idf_svc::http::server::EspHttpServer;
use esp_idf_svc::sys::EspError;
use esp_idf_svc::ws::FrameType;
use log::{info, warn};

use crate::console;

/// The open sessions' console connections, by the WebSocket's socket.
static SESSIONS: Mutex<Vec<(i32, TcpStream)>> = Mutex::new(Vec::new());

pub fn register(server: &mut EspHttpServer<'static>) -> Result<(), EspError> {
    server.ws_handler("/api/console", None, |ws: &mut EspHttpWsConnection| -> Result<(), String> {
        if ws.is_new() {
            open(ws)
        } else if ws.is_closed() {
            close(ws.session());
            Ok(())
        } else {
            input(ws)
        }
    })?;
    Ok(())
}

fn open(ws: &EspHttpWsConnection) -> Result<(), String> {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, console::PORT)).map_err(|err| format!("terminal: {err}"))?;
    let _ = stream.set_nodelay(true);
    let mut from_camera = stream.try_clone().map_err(|err| format!("terminal: {err}"))?;
    let mut sender = ws.create_detached_sender().map_err(|err| format!("terminal: {err}"))?;
    let session = ws.session();
    thread::Builder::new()
        .name("terminal".into())
        .stack_size(3 * 1024)
        .spawn(move || {
            let mut buf = [0u8; 512];
            let why = loop {
                match from_camera.read(&mut buf) {
                    Ok(0) => break "console closed",
                    Ok(read) => {
                        if sender.send(FrameType::Binary(false), &buf[..read]).is_err() {
                            break "";
                        }
                    }
                    Err(_) => break "console closed",
                }
            };
            if !why.is_empty() {
                let _ = sender.send(FrameType::Text(false), why.as_bytes());
            }
            close(session);
        })
        .map_err(|err| format!("terminal: {err}"))?;
    SESSIONS.lock().unwrap_or_else(PoisonError::into_inner).push((session, stream));
    info!("terminal: session {session} opened");
    Ok(())
}

/// Ends a session's console connection, which ends its reader thread; done by whichever of
/// the page and the console goes first.
fn close(session: i32) {
    let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(at) = sessions.iter().position(|(id, _)| *id == session) {
        let (_, stream) = sessions.swap_remove(at);
        let _ = stream.shutdown(Shutdown::Both);
        info!("terminal: session {session} closed");
    }
}

fn input(ws: &mut EspHttpWsConnection) -> Result<(), String> {
    // A zero-length buffer asks for the frame's size without taking it.
    let (_, len) = ws.recv(&mut []).map_err(|err| format!("terminal: {err}"))?;
    let mut bytes = vec![0u8; len];
    let (kind, len) = ws.recv(&mut bytes).map_err(|err| format!("terminal: {err}"))?;
    if !matches!(kind, FrameType::Binary(_) | FrameType::Text(_)) || len == 0 {
        return Ok(());
    }
    let session = ws.session();
    let mut sessions = SESSIONS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((_, stream)) = sessions.iter_mut().find(|(id, _)| *id == session) {
        if let Err(err) = stream.write_all(&bytes[..len]) {
            warn!("terminal: session {session}: {err}");
        }
    }
    Ok(())
}
