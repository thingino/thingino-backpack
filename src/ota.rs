//! Firmware updates over HTTP: a release's app image (`thingino-backpack-<chip>-app.bin`)
//! goes into whichever of the two OTA slots is not running, ESP-IDF verifies it, and the
//! unit restarts into it.
//!
//! A new firmware runs on probation. Once the unit has an address on its network it marks
//! itself valid; a reset before that, or no address within [`CONFIRM_WITHIN`], and the
//! bootloader goes back to the firmware it came from.

use core::ffi::CStr;
use core::fmt::Debug;
use core::ptr;
use core::time::Duration;
use std::thread;
use std::time::Instant;

use esp_idf_svc::io::Read;
use esp_idf_svc::ota::{EspOta, SlotState};
use esp_idf_svc::sys;
use log::{info, warn};

/// A Wi-Fi join with its retries, DHCP and SLAAC, with room for a slow network.
const CONFIRM_WITHIN: Duration = Duration::from_secs(300);

const CHUNK: usize = 4096;

/// What an app image starts with: the image magic, its chip at byte 12, and the app
/// description's magic word at byte 32. A bootloader, and so a first-install image, has a
/// different word there.
const IMAGE_MAGIC: u8 = 0xE9;
const CHIP_ID_AT: usize = 12;
const APP_DESC_AT: usize = 32;
const APP_DESC_MAGIC: u32 = 0xABCD_5432;

/// The firmware running, as the status page and `GET /api/ota` report it.
pub struct Running {
    pub slot: String,
    pub state: State,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Confirmed after an update.
    Valid,
    /// Updated, and not confirmed yet.
    Probation,
    /// Written over USB: no OTA state at all.
    Flashed,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::Probation => "on probation",
            Self::Flashed => "flashed",
        }
    }
}

pub fn running() -> Running {
    // SAFETY: the running partition is an entry of the partition table ESP-IDF read at
    // boot, valid for as long as the firmware runs.
    let partition = unsafe { &*sys::esp_ota_get_running_partition() };
    let mut state: sys::esp_ota_img_states_t = 0;
    // SAFETY: as above; `state` outlives the call.
    let found = unsafe { sys::esp_ota_get_state_partition(partition, &raw mut state) };
    let state = match (found, state) {
        (0, sys::esp_ota_img_states_t_ESP_OTA_IMG_VALID) => State::Valid,
        (0, sys::esp_ota_img_states_t_ESP_OTA_IMG_PENDING_VERIFY | sys::esp_ota_img_states_t_ESP_OTA_IMG_NEW) => {
            State::Probation
        }
        _ => State::Flashed,
    };
    Running {
        slot: label(partition),
        state,
    }
}

/// On a new firmware's first boot, marks it valid once `reachable`, or goes back to the
/// previous firmware if it is not within [`CONFIRM_WITHIN`].
pub fn confirm_when_reachable(reachable: fn() -> bool) {
    let unverified = EspOta::new()
        .and_then(|ota| ota.get_running_slot())
        .is_ok_and(|slot| matches!(slot.state, SlotState::Unverified));
    if !unverified {
        return;
    }
    info!(
        "ota: new firmware on probation until the unit has an address, {} s at most",
        CONFIRM_WITHIN.as_secs()
    );
    let watch = move || {
        let deadline = Instant::now() + CONFIRM_WITHIN;
        while Instant::now() < deadline {
            if reachable() {
                // An update request holds the only `EspOta` while it runs.
                match EspOta::new().and_then(|mut ota| ota.mark_running_slot_valid()) {
                    Ok(()) => return info!("ota: new firmware confirmed"),
                    Err(err) => warn!("ota: confirming: {err}"),
                }
            }
            thread::sleep(Duration::from_secs(1));
        }
        warn!("ota: no address in {} s; going back to the previous firmware", CONFIRM_WITHIN.as_secs());
        match EspOta::new() {
            Ok(mut ota) => warn!("ota: rollback failed: {}", ota.mark_running_slot_invalid_and_reboot()),
            Err(err) => warn!("ota: rollback: {err}"),
        }
    };
    if let Err(err) = crate::spawn_named(c"ota-confirm", 4096, watch) {
        warn!("ota: {err}");
    }
}

/// Writes the app image `body`, `len` bytes, into the slot not running, verifies it and
/// makes it the one to boot, answering what to tell the user. Whatever the outcome, the
/// whole body is read.
pub fn update<R: Read>(body: &mut R, len: usize) -> Result<String, String>
where
    R::Error: Debug,
{
    let mut buf = vec![0_u8; CHUNK];
    let result = write(body, len, &mut buf);
    if let Err(at) = result.as_ref().map_err(|(at, _)| *at) {
        // The client sends the rest anyway; reading it lets the answer reach it cleanly.
        let mut left = len - at;
        while left > 0 {
            match body.read(&mut buf[..left.min(CHUNK)]) {
                Ok(0) | Err(_) => break,
                Ok(n) => left -= n,
            }
        }
    }
    result.map_err(|(_, why)| why)
}

/// The update itself; an error comes with how much of the body was read before it.
fn write<R: Read>(body: &mut R, len: usize, buf: &mut [u8]) -> Result<String, (usize, String)>
where
    R::Error: Debug,
{
    // The other slot holds what a firmware on probation would go back to.
    if running().state == State::Probation {
        return Err((0, "the running firmware has not confirmed itself yet; try again once it has".into()));
    }
    let mut ota = EspOta::new().map_err(|_| (0, "another update is running".to_owned()))?;
    // SAFETY: a plain lookup in the partition table.
    let slot = unsafe { sys::esp_ota_get_next_update_partition(ptr::null()) };
    if slot.is_null() {
        return Err((0, "this firmware has no slot to update into; flash a first-install image once".into()));
    }
    // SAFETY: a partition table entry, valid for as long as the firmware runs.
    let slot = unsafe { &*slot };
    let capacity = usize::try_from(slot.size).unwrap_or(usize::MAX);
    if len > capacity {
        return Err((0, format!("{len} bytes do not fit the {capacity}-byte slot")));
    }
    // The first piece is checked before the slot is erased, so a wrong file costs nothing.
    let mut n = fill(body, &mut buf[..len.min(CHUNK)]).map_err(|err| (0, format!("receiving the image: {err:?}")))?;
    check_header(&buf[..n]).map_err(|why| (n, why))?;
    let mut update = ota
        .initiate_update_with_known_size(len)
        .map_err(|err| (n, format!("starting the update: {err}")))?;
    let mut done = 0;
    loop {
        update.write(&buf[..n]).map_err(|err| (done + n, format!("writing the image: {err}")))?;
        done += n;
        if done == len {
            break;
        }
        n = fill(body, &mut buf[..(len - done).min(CHUNK)])
            .map_err(|err| (done, format!("receiving the image: {err:?}")))?;
        if n == 0 {
            return Err((done, format!("the image ended after {done} of {len} bytes")));
        }
    }
    update
        .complete()
        .map_err(|err| (done, format!("the image did not verify ({err}); this firmware stays")))?;
    let slot = label(slot);
    info!("ota: {len} bytes into {slot}; restarting into them");
    crate::restart_soon();
    Ok(format!("{len} bytes written to {slot}; restarting into the new firmware"))
}

/// Reads until `buf` is full or the body ends, answering how much it read.
fn fill<R: Read>(body: &mut R, buf: &mut [u8]) -> Result<usize, R::Error> {
    let mut got = 0;
    while got < buf.len() {
        let n = body.read(&mut buf[got..])?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// Refuses what is not an app image for this chip, before anything is written.
fn check_header(head: &[u8]) -> Result<(), String> {
    let word = |at: usize| head.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if head.first() != Some(&IMAGE_MAGIC) {
        return Err("this is not an ESP32 firmware image".into());
    }
    if word(APP_DESC_AT) != Some(APP_DESC_MAGIC) {
        return Err("this is not an app image (a first-install image, perhaps?): take the -app.bin".into());
    }
    let chip = head.get(CHIP_ID_AT..CHIP_ID_AT + 2).map_or(u16::MAX, |b| u16::from_le_bytes([b[0], b[1]]));
    if u32::from(chip) != sys::CONFIG_IDF_FIRMWARE_CHIP_ID {
        let this = CStr::from_bytes_until_nul(sys::CONFIG_IDF_TARGET).map_or("this chip".into(), CStr::to_string_lossy);
        return Err(format!("this image is for {}, not for this {this}", chip_name(chip)));
    }
    Ok(())
}

/// ESP-IDF's numbers for the chips the backpack builds for.
fn chip_name(id: u16) -> String {
    match id {
        2 => "an esp32s2".into(),
        9 => "an esp32s3".into(),
        18 => "an esp32p4".into(),
        id => format!("chip {id}"),
    }
}

fn label(partition: &sys::esp_partition_t) -> String {
    // SAFETY: a partition label is a NUL-terminated array of its own.
    unsafe { CStr::from_ptr(partition.label.as_ptr()) }.to_string_lossy().into_owned()
}
