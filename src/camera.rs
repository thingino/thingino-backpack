//! The camera's power and boot pin.
//!
//! GPIO15 switches the camera's supply through a MOSFET module, high for on. GPIO16 goes to
//! the camera's flash pin 5 and is open-drain, so it only ever pulls low or lets go: low
//! holds the flash's data input at ground, the bootrom cannot read its SPL and falls back
//! to USB boot; released, the flash works as before.
//!
//! What takes time runs on a thread of its own: power cycles, entering the bootrom (hold
//! the pin, cycle the power, let the pin go as soon as the bootrom enumerates, since U-Boot
//! needs the flash), and recovery, which power-cycles a camera that stopped serving USB
//! without leaving the bus or that the port cannot get enumerated.
//!
//! The flash programmer borrows the flash chip, and with it the boot pin as its MOSI, only
//! from a camera that is off; until it gives them back the camera stays off.

use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Instant;

use esp_idf_svc::hal::gpio::OutputPin;
use esp_idf_svc::sys::{self, esp};
use log::{info, warn};
use tdfu_usb::espidf::UsbHost;

use crate::console;

/// Long enough for the camera's supply to drain, so a power cycle is a cold boot.
const OFF_FOR: Duration = Duration::from_secs(1);
/// The console's TX drives the camera's RX again this long after power-on, not while the
/// camera comes out of reset.
const TX_AFTER: Duration = Duration::from_millis(300);
/// The bootrom enumerates about two seconds after power-on.
const BOOTROM_WITHIN: Duration = Duration::from_secs(10);
/// No bootrom enumerates this soon after power-on, so a device on the bus by then is one
/// that never lost power.
const NEVER_LEFT: Duration = Duration::from_millis(300);
/// A device kept from closing this long by a control transfer it never answered is dead.
const STUCK_LIMIT: Duration = Duration::from_secs(10);
/// Port power cycles in a row that did not get the camera enumerated.
const FAILED_ENUMERATIONS: u32 = 3;
const FIRST_COOLDOWN: Duration = Duration::from_secs(30);
const LONGEST_COOLDOWN: Duration = Duration::from_secs(600);
/// RFC 2217 clients set DTR and RTS one request at a time, and the states in between
/// (the boot pin held for a moment while a terminal opens) must not reach a camera that is
/// reading its flash, so the lines act once they have held still this long.
const LINES_SETTLE: Duration = Duration::from_millis(100);
const TICK: Duration = Duration::from_millis(100);
const RECOVERY_EVERY: Duration = Duration::from_millis(500);
const ANSWER_WITHIN: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PowerOn,
    PowerOff,
    PowerCycle,
    Bootrom,
    BootHold,
    BootRelease,
}

impl Action {
    pub const NAMES: &str = "power-on, power-off, power-cycle, bootrom, boot-hold or boot-release";

    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "power-on" => Some(Self::PowerOn),
            "power-off" => Some(Self::PowerOff),
            "power-cycle" => Some(Self::PowerCycle),
            "bootrom" => Some(Self::Bootrom),
            "boot-hold" => Some(Self::BootHold),
            "boot-release" => Some(Self::BootRelease),
            _ => None,
        }
    }
}

struct Pins {
    power: i32,
    boot: i32,
}

struct State {
    powered: bool,
    boot_held: bool,
    tx_at: Option<Instant>,
    /// Power came on with the boot pin held at this time: the pin goes as soon as the
    /// bootrom enumerates.
    boot_watch: Option<Instant>,
    /// How the last boot watch ended, for a bootrom request waiting on it: the time the
    /// bootrom took, or what went wrong.
    boot_outcome: Option<Result<Duration, String>>,
    /// The RFC 2217 lines (DTR, RTS) as last set, since when, and as last acted on.
    lines: (bool, bool),
    lines_since: Instant,
    lines_applied: (bool, bool),
    /// What the lines hold asserted.
    lines_off: bool,
    lines_boot: bool,
    /// The flash programmer has the flash chip and the boot pin.
    flash_lent: bool,
}

impl State {
    fn power(&mut self, pins: &Pins, on: bool) {
        // SAFETY: the pin was configured as an output in `start` and nothing else drives it.
        unsafe { sys::gpio_set_level(pins.power, u32::from(on)) };
        if self.powered != on {
            info!("camera: power {}", if on { "on" } else { "off" });
        }
        self.powered = on;
        if on {
            self.tx_at = Some(Instant::now() + TX_AFTER);
            if self.boot_held {
                self.boot_watch = Some(Instant::now());
                self.boot_outcome = None;
            }
        } else {
            self.tx_at = None;
            self.boot_watch = None;
            console::set_tx(false);
        }
    }

    fn boot(&mut self, pins: &Pins, held: bool) {
        // SAFETY: as in `power`; the pin is open-drain, so high is released.
        unsafe { sys::gpio_set_level(pins.boot, u32::from(!held)) };
        if self.boot_held != held {
            info!("camera: boot pin {}", if held { "held" } else { "released" });
        }
        self.boot_held = held;
        if !held {
            self.boot_watch = None;
        }
    }
}

pub struct Status {
    pub powered: bool,
    pub boot_held: bool,
    pub enumerated: usize,
    pub stuck_for: Option<Duration>,
    pub recoveries: u32,
    pub flash_lent: bool,
}

/// The flash chip, lent to the flash programmer: the camera stays off, and the boot pin is
/// the programmer's, until this is dropped.
pub struct FlashLease<'a> {
    camera: &'a Camera,
}

impl Drop for FlashLease<'_> {
    fn drop(&mut self) {
        self.camera.return_flash();
    }
}

type Request = (Action, SyncSender<Result<String, String>>);

pub struct Camera {
    pins: Pins,
    state: Mutex<State>,
    host: UsbHost,
    requests: Sender<Request>,
    recoveries: AtomicU32,
}

/// Takes the two pins, powers the camera and starts the thread that runs requests and
/// recovery. The camera powered off while the ESP32 was in reset, so this is a power-on.
pub fn start(power: impl OutputPin + 'static, boot: impl OutputPin + 'static, host: UsbHost) -> Result<Arc<Camera>, String> {
    let pins = Pins {
        power: i32::from(power.pin()),
        boot: i32::from(boot.pin()),
    };
    // The level goes in before the pin becomes an output, so the boot pin never pulls the
    // flash low for an instant while the camera may be using it.
    configure(pins.boot, sys::gpio_mode_t_GPIO_MODE_OUTPUT_OD, 1).map_err(|err| format!("boot pin: {err}"))?;
    // It sinks against the SoC's own driver on that line, so at full strength.
    // SAFETY: the pin is configured above.
    esp!(unsafe { sys::gpio_set_drive_capability(pins.boot, sys::gpio_drive_cap_t_GPIO_DRIVE_CAP_3) })
        .map_err(|err| format!("boot pin: {err}"))?;
    configure(pins.power, sys::gpio_mode_t_GPIO_MODE_OUTPUT, 1).map_err(|err| format!("power pin: {err}"))?;
    info!("camera: power on GPIO{}, boot pin on GPIO{}", pins.power, pins.boot);

    let (requests, received) = mpsc::channel();
    let camera = Arc::new(Camera {
        pins,
        state: Mutex::new(State {
            powered: true,
            boot_held: false,
            tx_at: Some(Instant::now() + TX_AFTER),
            boot_watch: None,
            boot_outcome: None,
            lines: (false, false),
            lines_since: Instant::now(),
            lines_applied: (false, false),
            lines_off: false,
            lines_boot: false,
            flash_lent: false,
        }),
        host,
        requests,
        recoveries: AtomicU32::new(0),
    });
    let supervisor = Arc::clone(&camera);
    crate::spawn_named(c"camera", 4096, move || supervise(&supervisor, &received))
        .map_err(|err| format!("camera: {err}"))?;
    Ok(camera)
}

#[expect(
    clippy::field_reassign_with_default,
    reason = "the P4's configuration has a field the S2's and S3's lack, so no one struct literal fits all three"
)]
pub(crate) fn configure(pin: i32, mode: sys::gpio_mode_t, level: u32) -> Result<(), sys::EspError> {
    // SAFETY: the pin number comes from a peripheral this module took ownership of.
    esp!(unsafe { sys::gpio_set_level(pin, level) })?;
    let mut config = sys::gpio_config_t::default();
    config.pin_bit_mask = 1_u64 << pin;
    config.mode = mode;
    config.pull_up_en = sys::gpio_pullup_t_GPIO_PULLUP_DISABLE;
    config.pull_down_en = sys::gpio_pulldown_t_GPIO_PULLDOWN_DISABLE;
    config.intr_type = sys::gpio_int_type_t_GPIO_INTR_DISABLE;
    // SAFETY: `config` outlives the call.
    esp!(unsafe { sys::gpio_config(&config) })
}

impl Camera {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The power and boot pin GPIOs.
    pub fn pins(&self) -> (i32, i32) {
        (self.pins.power, self.pins.boot)
    }

    pub fn status(&self) -> Status {
        let (powered, boot_held, flash_lent) = {
            let state = self.state();
            (state.powered, state.boot_held, state.flash_lent)
        };
        Status {
            powered,
            boot_held,
            enumerated: self.host.enumerated().len(),
            stuck_for: self.host.stuck_for(),
            recoveries: self.recoveries.load(Ordering::Relaxed),
            flash_lent,
        }
    }

    /// Lends the flash chip to the flash programmer, if the camera is off: its power
    /// switch off, nothing of it on the USB bus, and its UART TX not held high, which an idle
    /// UART does whenever the camera has power. The last two catch a switch that is not
    /// cutting the supply.
    pub fn lend_flash(&self) -> Result<FlashLease<'_>, String> {
        {
            let mut state = self.state();
            if state.flash_lent {
                return Err("the flash chip is already lent".into());
            }
            if state.powered {
                return Err("the camera is on; switch it off first".into());
            }
            if state.boot_held || state.boot_watch.is_some() {
                return Err("the boot pin is held".into());
            }
            // From here the camera refuses power, so it cannot come on during the checks.
            state.flash_lent = true;
        }
        let checked = if !self.host.enumerated().is_empty() {
            Err("the camera is on the USB bus, so it still has power")
        } else if console::camera_tx_high() {
            Err("the camera's UART TX is high, so it still has power")
        } else {
            Ok(())
        };
        if let Err(why) = checked {
            self.state().flash_lent = false;
            return Err(why.into());
        }
        info!("camera: flash chip lent to the flash programmer");
        Ok(FlashLease { camera: self })
    }

    fn return_flash(&self) {
        let mut state = self.state();
        // The SPI bus had the boot pin: back to open-drain and released, as `start` left it.
        if let Err(err) = configure(self.pins.boot, sys::gpio_mode_t_GPIO_MODE_OUTPUT_OD, 1) {
            warn!("camera: boot pin: {err}");
        }
        // SAFETY: the pin is configured just above.
        unsafe { sys::gpio_set_drive_capability(self.pins.boot, sys::gpio_drive_cap_t_GPIO_DRIVE_CAP_3) };
        state.flash_lent = false;
        info!("camera: flash chip back from the flash programmer");
    }

    /// Runs `action` on the camera thread and answers what came of it.
    pub fn request(&self, action: Action) -> Result<String, String> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.requests
            .send((action, reply))
            .map_err(|_| "the camera thread is gone".to_owned())?;
        answer
            .recv_timeout(ANSWER_WITHIN)
            .map_err(|_| "the camera thread did not answer".to_owned())?
    }

    /// The console's RFC 2217 control lines, read the way esptool's auto-reset circuit reads
    /// them: RTS without DTR cuts the power (EN), DTR without RTS holds the boot pin (IO0),
    /// and both or neither leave the camera alone, so a terminal that asserts both when it
    /// opens changes nothing. They act once settled; only what they asserted is undone.
    pub fn lines(&self, dtr: bool, rts: bool) {
        let mut state = self.state();
        if state.lines != (dtr, rts) {
            state.lines = (dtr, rts);
            state.lines_since = Instant::now();
        }
    }

    fn act(&self, action: Action) -> Result<String, String> {
        if action != Action::PowerOff && self.state().flash_lent {
            return Err("the flash chip is lent to the flash programmer, and the camera stays off until flashrom is done"
                .into());
        }
        match action {
            Action::PowerOn => {
                self.state().power(&self.pins, true);
                Ok("power on".into())
            }
            Action::PowerOff => {
                self.state().power(&self.pins, false);
                Ok("power off".into())
            }
            Action::PowerCycle => {
                self.cycle();
                Ok("power cycled".into())
            }
            Action::Bootrom => self.bootrom(),
            Action::BootHold => {
                self.state().boot(&self.pins, true);
                Ok("boot pin held".into())
            }
            Action::BootRelease => {
                self.state().boot(&self.pins, false);
                Ok("boot pin released".into())
            }
        }
    }

    fn cycle(&self) {
        self.state().power(&self.pins, false);
        self.wait(OFF_FOR);
        self.state().power(&self.pins, true);
    }

    fn bootrom(&self) -> Result<String, String> {
        {
            let mut state = self.state();
            state.boot(&self.pins, true);
            state.power(&self.pins, false);
        }
        self.wait(OFF_FOR);
        // A device still on the bus never lost power, and would pass for the bootrom.
        if !self.host.enumerated().is_empty() {
            let mut state = self.state();
            state.power(&self.pins, true);
            state.boot(&self.pins, false);
            return Err("the camera stayed on the USB bus with its power off, so the power switch is not \
                        cutting its supply; boot pin released"
                .into());
        }
        self.state().power(&self.pins, true);
        let deadline = Instant::now() + BOOTROM_WITHIN + TICK * 5;
        while Instant::now() < deadline {
            self.housekeeping();
            if let Some(outcome) = self.state().boot_outcome.take() {
                return outcome
                    .map(|after| {
                        format!(
                            "the bootrom enumerated {} ms after power-on; boot pin released",
                            after.as_millis()
                        )
                    })
                    .map_err(|why| format!("{why}; boot pin released"));
            }
            thread::sleep(Duration::from_millis(20));
        }
        self.state().boot(&self.pins, false);
        Err("the boot pin was let go before the bootrom was seen".into())
    }

    /// Sleeps for `period` while keeping up with the scheduled work.
    fn wait(&self, period: Duration) {
        let until = Instant::now() + period;
        while Instant::now() < until {
            thread::sleep(Duration::from_millis(20).min(until - Instant::now()));
            self.housekeeping();
        }
    }

    /// Whatever is due: settled RFC 2217 lines, TX back on after power-on, and the boot pin
    /// let go once the bootrom enumerates.
    fn housekeeping(&self) {
        let now = Instant::now();
        let mut state = self.state();
        // Lines set while the flash is lent act once it is back.
        if !state.flash_lent && state.lines != state.lines_applied && state.lines_since.elapsed() >= LINES_SETTLE {
            let (dtr, rts) = state.lines;
            state.lines_applied = (dtr, rts);
            let (off, boot) = (rts && !dtr, dtr && !rts);
            // The boot pin first: from reset straight to boot, power comes back with it held.
            if boot != state.lines_boot {
                state.lines_boot = boot;
                state.boot(&self.pins, boot);
            }
            if off != state.lines_off {
                state.lines_off = off;
                state.power(&self.pins, !off);
            }
        }
        if state.tx_at.is_some_and(|at| now >= at) {
            state.tx_at = None;
            console::set_tx(true);
        }
        if let Some(since) = state.boot_watch {
            let elapsed = since.elapsed();
            let enumerated = !self.host.enumerated().is_empty();
            if enumerated && elapsed < NEVER_LEFT {
                let why = "the camera stayed on the USB bus through the power cut, so the power switch is not cutting its supply";
                warn!("camera: {why}");
                state.boot(&self.pins, false);
                state.boot_outcome = Some(Err(why.to_owned()));
            } else if enumerated {
                info!("camera: the bootrom enumerated {} ms after power-on", elapsed.as_millis());
                state.boot(&self.pins, false);
                state.boot_outcome = Some(Ok(elapsed));
            } else if elapsed >= BOOTROM_WITHIN {
                let why = format!("nothing enumerated within {} s of power-on", elapsed.as_secs());
                warn!("camera: {why} with the boot pin held");
                state.boot(&self.pins, false);
                state.boot_outcome = Some(Err(why));
            }
        }
    }
}

fn supervise(camera: &Camera, requests: &Receiver<Request>) {
    let mut recovery = Recovery {
        next_check: Instant::now(),
        quiet_until: None,
        cooldown: FIRST_COOLDOWN,
        last: None,
    };
    loop {
        match requests.recv_timeout(TICK) {
            Ok((action, reply)) => {
                let _ = reply.send(camera.act(action));
            }
            Err(RecvTimeoutError::Timeout) => {}
            // The camera holds a sender for as long as it exists, which is for ever.
            Err(RecvTimeoutError::Disconnected) => thread::sleep(TICK),
        }
        camera.housekeeping();
        recovery.check(camera);
    }
}

/// Power cycles for a camera the USB side has given up on, backing off from thirty seconds
/// to ten minutes while it keeps needing them.
struct Recovery {
    next_check: Instant,
    quiet_until: Option<Instant>,
    cooldown: Duration,
    last: Option<Instant>,
}

impl Recovery {
    fn check(&mut self, camera: &Camera) {
        let now = Instant::now();
        if now < self.next_check {
            return;
        }
        self.next_check = now + RECOVERY_EVERY;
        if self.quiet_until.is_some_and(|until| now < until) {
            return;
        }
        if self.last.is_some_and(|last| now.duration_since(last) > LONGEST_COOLDOWN * 2) {
            self.cooldown = FIRST_COOLDOWN;
        }
        {
            // Power that is meant to be off stays off, and a bootrom entry is not cut short.
            let state = camera.state();
            if !state.powered || state.boot_watch.is_some() {
                return;
            }
        }
        let reason = if let Some(stuck) = camera.host.stuck_for().filter(|stuck| *stuck >= STUCK_LIMIT) {
            format!("a USB control transfer has gone unanswered for {} s", stuck.as_secs())
        } else if camera.host.failed_enumerations() >= FAILED_ENUMERATIONS {
            format!(
                "{} port power cycles have not got it enumerated",
                camera.host.failed_enumerations()
            )
        } else {
            return;
        };
        warn!("camera: {reason}; power-cycling it");
        camera.cycle();
        camera.recoveries.fetch_add(1, Ordering::Relaxed);
        self.last = Some(now);
        self.quiet_until = Some(Instant::now() + self.cooldown);
        self.cooldown = (self.cooldown * 2).min(LONGEST_COOLDOWN);
    }
}
