//! The camera's USB lines, which the backpack's host port can let go of. A camera whose SoC
//! hosts a USB Wi-Fi module on those same lines cannot have a second host on them: the port
//! powers down, so the backpack stops resetting and enumerating, and on the S2 and S3 its PHY
//! lets go of the lines too, its pull resistors off and its pads disconnected.

#[cfg(not(esp32p4))]
use core::ptr;
#[cfg(not(esp32p4))]
use std::sync::{Mutex, PoisonError};

use esp_idf_svc::sys::{self, EspError};

/// The USB wrapper's OTG configuration, the first of its registers.
#[cfg(esp32s3)]
const OTG_CONF: usize = 0x6003_9000;
#[cfg(esp32s2)]
const OTG_CONF: usize = 0x3F43_9000;
/// The PHY's pull override, the four pulls it sets (D+ up and down, D- up and down), and the
/// pads' connection to the PHY.
#[cfg(not(esp32p4))]
const PULL_OVERRIDE: u32 = 1 << 12;
#[cfg(not(esp32p4))]
const PULLS: u32 = 0b1111 << 13;
#[cfg(not(esp32p4))]
const PAD_ENABLE: u32 = 1 << 18;

/// The configuration the PHY was set up with, put back when the lines are taken again.
#[cfg(not(esp32p4))]
static CONNECTED: Mutex<Option<u32>> = Mutex::new(None);

/// Takes the camera's USB lines, or lets go of them.
pub fn connect(on: bool) -> Result<(), EspError> {
    if on {
        #[cfg(not(esp32p4))]
        hold_lines(true);
        power(true)
    } else {
        power(false)?;
        #[cfg(not(esp32p4))]
        hold_lines(false);
        Ok(())
    }
}

/// Powers the root port up or down; one that already is stays so.
fn power(on: bool) -> Result<(), EspError> {
    // SAFETY: the USB host library is installed before the camera starts, and never removed.
    let code = unsafe { sys::usb_host_lib_set_root_port_power(on) };
    match EspError::from(code) {
        Some(err) if err.code() != sys::ESP_ERR_INVALID_STATE => Err(err),
        _ => Ok(()),
    }
}

/// Puts the PHY's pulls and pads back as they were set up, or saves them and lets go of the
/// lines: every pull off and the pads disconnected from the PHY, which leaves D+ and D- to
/// the camera. Nothing else writes the register once the library is installed.
#[cfg(not(esp32p4))]
fn hold_lines(hold: bool) {
    let conf = ptr::with_exposed_provenance_mut::<u32>(OTG_CONF);
    let mut saved = CONNECTED.lock().unwrap_or_else(PoisonError::into_inner);
    if hold {
        if let Some(was) = saved.take() {
            // SAFETY: the chip's own register, written whole as it was read.
            unsafe { ptr::write_volatile(conf, was) };
        }
    } else {
        // SAFETY: as above.
        let now = unsafe { ptr::read_volatile(conf) };
        let was = *saved.get_or_insert(now);
        // SAFETY: as above.
        unsafe { ptr::write_volatile(conf, (was | PULL_OVERRIDE) & !(PULLS | PAD_ENABLE)) };
    }
}
