//! A flash chip programmer for flashrom: its serprog protocol on [`PORT`], driving the
//! camera's SPI NOR flash through a SOIC-8 clip while the camera is off, or parked in its
//! bootrom.
//!
//! flashrom does the chip handling (probing, erasing, writing, verifying), each step one SPI
//! operation and one network round trip:
//!
//! ```text
//! flashrom -p serprog:ip=<host>:8888 -r dump.bin
//! ```
//!
//! The clip is driven only while flashrom has its pins enabled (`S_PIN_STATE`), and only
//! once the camera module has lent the flash chip, the boot pin, already on the flash's DI,
//! handed over as MOSI. Enabling powers the chip's VCC, then drives its HOLD high so that
//! nothing can pause it, then the bus; disabling, a client that leaves, or one quiet for
//! [`IDLE_LIMIT`] puts every pin back to high-Z. WP stays unconnected: a chip heeds it only to
//! keep its status register locked, and only with quad mode off. On a camera's board, VCC is
//! its 3.3 V rail and HOLD one of the SoC's quad data lines, so a backpack soldered to the
//! flash leaves them to the camera outside a session, and the camera boots as if it were not
//! there. A clip on a bare chip can take its VCC from the unit's 3.3 V, with HOLD tied to it,
//! instead.
//!
//! VCC and HOLD are settings, each driven or left alone, saved in NVS. Driven, VCC needs the
//! camera off and staying off. Left alone, the chip runs on the camera's own supply: for
//! boards where that rail also runs the SoC, whose bootrom would take the flash's pins as soon
//! as the clip powered it, the camera is parked in its bootrom for the session instead. HOLD
//! left alone suits chips in quad mode, which ignore it, and boards that pull it up.
//!
//! The protocol is flashrom's own specification (serprog-protocol.rst), version 1, with the
//! commands flashrom uses on an SPI-only programmer.

use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;
use std::io::{self, Read, Write};
use std::net::{Ipv6Addr, TcpListener, TcpStream};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Instant;

use esp_idf_svc::hal::gpio::{InputPin, OutputPin};
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs};
use esp_idf_svc::sys::{self, esp, EspError};
use log::{info, warn};

use crate::camera::{self, Camera, FlashLease};

pub const PORT: u16 = 8888;

const ACK: u8 = 0x06;
const NAK: u8 = 0x15;

const NOP: u8 = 0x00;
const Q_IFACE: u8 = 0x01;
const Q_CMDMAP: u8 = 0x02;
const Q_PGMNAME: u8 = 0x03;
const Q_SERBUF: u8 = 0x04;
const Q_BUSTYPE: u8 = 0x05;
const Q_WRNMAXLEN: u8 = 0x08;
const SYNCNOP: u8 = 0x10;
const Q_RDNMAXLEN: u8 = 0x11;
const S_BUSTYPE: u8 = 0x12;
const O_SPIOP: u8 = 0x13;
const S_SPI_FREQ: u8 = 0x14;
const S_PIN_STATE: u8 = 0x15;

const COMMANDS: [u8; 13] = [
    NOP,
    Q_IFACE,
    Q_CMDMAP,
    Q_PGMNAME,
    Q_SERBUF,
    Q_BUSTYPE,
    Q_WRNMAXLEN,
    SYNCNOP,
    Q_RDNMAXLEN,
    S_BUSTYPE,
    O_SPIOP,
    S_SPI_FREQ,
    S_PIN_STATE,
];

const BUS_SPI: u8 = 1 << 3;

/// flashrom prints it as the programmer's name: 16 bytes, which it terminates itself.
const NAME: &[u8; 16] = b"backpack-serprog";

/// The DMA buffers an operation goes through, a piece at a time, with CS held low across
/// the pieces: flashrom's longest, a whole-chip read, never has to fit in memory.
const CHUNK: usize = 4096;

/// Answers this short go out in one segment with their ACK.
const SHORT_ANSWER: usize = 64;

const DEFAULT_HZ: u32 = 8_000_000;
/// Full-duplex SPI through the GPIO matrix tops out near 26 MHz, and clip leads well before.
const FASTEST_HZ: u32 = 20_000_000;
const SLOWEST_HZ: u32 = 100_000;
/// The SPI clock is this divided by a whole number, so every frequency offered is one.
const SPI_SOURCE_HZ: u32 = 80_000_000;

/// A bare flash is ready well within a millisecond of power-on.
const POWER_UP: Duration = Duration::from_millis(10);

/// On a board, VCC charges the board's capacitors too, and the chip answers 00s until they
/// are; a SoC on the same rail can then wake and take the flash's pins for a while. flashrom
/// asked for one chip probes it once, right away, so the chip is handed over once its ID has
/// read back the same for [`STEADY_FOR`], within [`READY_WITHIN`].
const READY_WITHIN: Duration = Duration::from_secs(5);
const STEADY_FOR: Duration = Duration::from_secs(1);

/// JEDEC Read Identification, which every flash this is for answers once it is up.
const RDID: u8 = 0x9F;

/// Where the settings are kept: 0 for left alone, anything else, or nothing, for driven.
const NAMESPACE: &str = "flash";
const VCC_DRIVEN: &str = "vcc_driven";
const HOLD_DRIVEN: &str = "hold_driven";

/// flashrom talks the whole time it runs; a client quiet this long is gone.
const IDLE_LIMIT: Duration = Duration::from_secs(30);

const STACK: usize = 5 * 1024;

const HOST: sys::spi_host_device_t = sys::spi_host_device_t_SPI2_HOST;

/// The clip's GPIOs. MOSI is the camera's boot pin.
#[derive(Clone, Copy)]
pub struct Pins {
    pub cs: i32,
    pub clk: i32,
    pub miso: i32,
    pub mosi: i32,
    pub vcc: i32,
    pub hold: i32,
}

static PINS: OnceLock<Pins> = OnceLock::new();
static NVS: OnceLock<EspDefaultNvsPartition> = OnceLock::new();
static DRIVE_VCC: AtomicBool = AtomicBool::new(true);
static DRIVE_HOLD: AtomicBool = AtomicBool::new(true);

/// The clip's GPIOs, once the programmer is started.
pub fn pins() -> Option<Pins> {
    PINS.get().copied()
}

/// Which of the chip's VCC and HOLD a session drives; the others it leaves alone.
#[derive(Clone, Copy)]
pub struct Driven {
    pub vcc: bool,
    pub hold: bool,
}

impl Driven {
    pub fn describe(self) -> String {
        format!(
            "VCC {}, HOLD {}",
            if self.vcc { "driven" } else { "left to the camera, parked in its bootrom for each session" },
            if self.hold { "driven high" } else { "left alone" }
        )
    }
}

pub fn driven() -> Driven {
    Driven {
        vcc: DRIVE_VCC.load(Ordering::Relaxed),
        hold: DRIVE_HOLD.load(Ordering::Relaxed),
    }
}

/// Sets whether sessions drive VCC and HOLD, each that is given, now and on every boot after;
/// a session already running keeps what it started with. Answers what to tell the user.
pub fn set_driven(vcc: Option<bool>, hold: Option<bool>) -> Result<String, String> {
    let nvs = NVS.get().ok_or("the flash programmer is not running")?;
    let store = EspNvs::new(nvs.clone(), NAMESPACE, true).map_err(|err| format!("opening NVS: {err}"))?;
    for (value, key, flag) in [(vcc, VCC_DRIVEN, &DRIVE_VCC), (hold, HOLD_DRIVEN, &DRIVE_HOLD)] {
        if let Some(value) = value {
            store.set_u8(key, u8::from(value)).map_err(|err| err.to_string())?;
            flag.store(value, Ordering::Relaxed);
        }
    }
    let now = driven().describe();
    info!("serprog: {now}");
    Ok(now)
}

/// Takes the clip's pins, leaves them high-Z, and serves flashrom on [`PORT`].
pub fn start(
    cs: impl OutputPin + 'static,
    clk: impl OutputPin + 'static,
    miso: impl InputPin + 'static,
    vcc: impl OutputPin + 'static,
    hold: impl OutputPin + 'static,
    camera: Arc<Camera>,
    nvs: EspDefaultNvsPartition,
) -> Result<(), String> {
    let pins = Pins {
        cs: i32::from(cs.pin()),
        clk: i32::from(clk.pin()),
        miso: i32::from(miso.pin()),
        mosi: camera.pins().1,
        vcc: i32::from(vcc.pin()),
        hold: i32::from(hold.pin()),
    };
    let failed = |err: EspError| format!("flash programmer: {err}");
    for pin in [pins.cs, pins.clk, pins.miso, pins.vcc, pins.hold] {
        release(pin).map_err(failed)?;
    }
    // Starting matters more than the settings.
    match EspNvs::new(nvs.clone(), NAMESPACE, true) {
        Ok(store) => {
            for (key, flag) in [(VCC_DRIVEN, &DRIVE_VCC), (HOLD_DRIVEN, &DRIVE_HOLD)] {
                match store.get_u8(key) {
                    Ok(saved) => flag.store(saved != Some(0), Ordering::Relaxed),
                    Err(err) => warn!("serprog: reading {key}: {err}"),
                }
            }
        }
        Err(err) => warn!("serprog: reading the settings: {err}"),
    }
    let listener = TcpListener::bind((Ipv6Addr::UNSPECIFIED, PORT)).map_err(|err| format!("flash programmer: {err}"))?;
    let _ = PINS.set(pins);
    let _ = NVS.set(nvs);
    info!(
        "serprog: flashrom on port {PORT}; clip CS GPIO{}, CLK GPIO{}, MISO GPIO{}, MOSI GPIO{}, VCC GPIO{}, HOLD GPIO{}; {}",
        pins.cs,
        pins.clk,
        pins.miso,
        pins.mosi,
        pins.vcc,
        pins.hold,
        driven().describe()
    );
    crate::spawn_named(c"serprog", STACK, move || serve(&listener, &pins, &camera))
        .map_err(|err| format!("flash programmer: {err}"))
}

/// One flashrom at a time: another waits in the listen backlog until the first is done.
fn serve(listener: &TcpListener, pins: &Pins, camera: &Camera) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            continue;
        };
        let peer = stream.peer_addr().map_or_else(|_| "a client".into(), |peer| peer.to_string());
        info!("serprog: {peer} connected");
        let mut session = Session {
            pins,
            camera,
            hz: DEFAULT_HZ,
            bus: None,
        };
        // flashrom waits on every answer, so each one goes out at once.
        let ended = stream
            .set_nodelay(true)
            .and_then(|()| stream.set_read_timeout(Some(IDLE_LIMIT)))
            .and_then(|()| session.run(&mut stream));
        // Before anything else: the pins go back to high-Z.
        drop(session);
        match ended {
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => info!("serprog: {peer} left"),
            Err(err) => info!("serprog: {peer} left: {err}"),
            Ok(()) => {}
        }
    }
}

struct Session<'a> {
    pins: &'a Pins,
    camera: &'a Camera,
    hz: u32,
    /// Present while flashrom has the pins enabled.
    bus: Option<Bus<'a>>,
}

impl Session<'_> {
    /// Answers commands until the client leaves or goes quiet, which is always an error.
    fn run(&mut self, stream: &mut TcpStream) -> io::Result<()> {
        loop {
            match byte(stream)? {
                NOP => stream.write_all(&[ACK])?,
                Q_IFACE => stream.write_all(&[ACK, 1, 0])?,
                Q_CMDMAP => {
                    let mut answer = [0_u8; 33];
                    answer[0] = ACK;
                    for command in COMMANDS {
                        answer[1 + usize::from(command / 8)] |= 1 << (command % 8);
                    }
                    stream.write_all(&answer)?;
                }
                Q_PGMNAME => {
                    let mut answer = [0_u8; 17];
                    answer[0] = ACK;
                    answer[1..].copy_from_slice(NAME);
                    stream.write_all(&answer)?;
                }
                // TCP is the flow control.
                Q_SERBUF => stream.write_all(&[ACK, 0xFF, 0xFF])?,
                Q_BUSTYPE => stream.write_all(&[ACK, BUS_SPI])?,
                // 0 is 2^24: an operation of any length streams through the chunks.
                Q_WRNMAXLEN | Q_RDNMAXLEN => stream.write_all(&[ACK, 0, 0, 0])?,
                SYNCNOP => stream.write_all(&[NAK, ACK])?,
                S_BUSTYPE => {
                    // More than one bit leaves the choice to the programmer, and SPI is all
                    // it has.
                    let buses = byte(stream)?;
                    stream.write_all(&[if buses & BUS_SPI == 0 { NAK } else { ACK }])?;
                }
                O_SPIOP => self.spi_op(stream)?,
                S_SPI_FREQ => {
                    let mut wanted = [0_u8; 4];
                    stream.read_exact(&mut wanted)?;
                    match self.set_hz(u32::from_le_bytes(wanted)) {
                        Some(hz) => {
                            let hz = hz.to_le_bytes();
                            stream.write_all(&[ACK, hz[0], hz[1], hz[2], hz[3]])?;
                        }
                        None => stream.write_all(&[NAK])?,
                    }
                }
                S_PIN_STATE => {
                    let enable = byte(stream)? != 0;
                    let answer = self.pin_state(enable);
                    stream.write_all(&[answer])?;
                }
                // flashrom sends only what the command map offers; anything else has
                // parameters of a length nobody here knows.
                _ => stream.write_all(&[NAK])?,
            }
        }
    }

    /// The fastest frequency at or below `wanted` that the SPI clock divides down to, within
    /// what clip leads allow; `None` for 0, which the protocol reserves.
    fn set_hz(&mut self, wanted: u32) -> Option<u32> {
        if wanted == 0 {
            return None;
        }
        let wanted = wanted.clamp(SLOWEST_HZ, FASTEST_HZ);
        let hz = SPI_SOURCE_HZ / SPI_SOURCE_HZ.div_ceil(wanted);
        if let Some(bus) = self.bus.as_mut() {
            if let Err(err) = bus.set_hz(hz) {
                warn!("serprog: SPI clock {hz} Hz: {err}");
                return None;
            }
        }
        self.hz = hz;
        Some(hz)
    }

    fn pin_state(&mut self, enable: bool) -> u8 {
        if !enable {
            self.bus = None;
            return ACK;
        }
        if self.bus.is_none() {
            match Bus::enable(self.pins, self.camera, self.hz) {
                Ok(bus) => {
                    info!("serprog: clip driven at {} kHz; {}", self.hz / 1000, bus.driven.describe());
                    self.bus = Some(bus);
                }
                Err(why) => {
                    warn!("serprog: not driving the clip: {why}");
                    return NAK;
                }
            }
        }
        ACK
    }

    /// `O_SPIOP`: with CS low, the bytes flashrom sent, then as many read back.
    fn spi_op(&mut self, stream: &mut TcpStream) -> io::Result<()> {
        let mut lengths = [0_u8; 6];
        stream.read_exact(&mut lengths)?;
        let write = u24(&lengths[..3]);
        let read = u24(&lengths[3..]);
        let Some(bus) = self.bus.as_mut() else {
            // The bytes are on the wire whatever the answer; they go, so the next command
            // lines up.
            discard(stream, write)?;
            return stream.write_all(&[NAK]);
        };
        bus.select();
        // What flashrom sends goes out as it arrives, and the answer follows all of it.
        let mut failed = None;
        let mut left = write;
        while left > 0 {
            let n = left.min(CHUNK);
            stream.read_exact(bus.tx.slice(n))?;
            if failed.is_none() {
                failed = bus.transfer(n, false).err();
            }
            left -= n;
        }
        if let Some(err) = failed {
            bus.deselect();
            warn!("serprog: SPI write: {err}");
            return stream.write_all(&[NAK]);
        }
        if read == 0 {
            bus.deselect();
            return stream.write_all(&[ACK]);
        }
        bus.tx.slice(read.min(CHUNK)).fill(0xFF);
        if read <= SHORT_ANSWER {
            let result = bus.transfer(read, true);
            bus.deselect();
            if let Err(err) = result {
                warn!("serprog: SPI read: {err}");
                return stream.write_all(&[NAK]);
            }
            let mut answer = [0_u8; 1 + SHORT_ANSWER];
            answer[0] = ACK;
            answer[1..=read].copy_from_slice(bus.rx.slice(read));
            return stream.write_all(&answer[..=read]);
        }
        // Longer reads stream: the ACK first, then the flash's bytes as they come in.
        stream.write_all(&[ACK])?;
        let mut left = read;
        while left > 0 {
            let n = left.min(CHUNK);
            if let Err(err) = bus.transfer(n, true) {
                bus.deselect();
                // Past the ACK, ending the session is the only way left to fail.
                return Err(io::Error::other(format!("SPI read: {err}")));
            }
            stream.write_all(bus.rx.slice(n))?;
            left -= n;
        }
        bus.deselect();
        Ok(())
    }
}

/// The SPI bus on the clip, for as long as flashrom has the pins enabled.
struct Bus<'a> {
    pins: &'a Pins,
    device: sys::spi_device_handle_t,
    tx: Dma,
    rx: Dma,
    driven: Driven,
    /// Last, so the camera takes the boot pin back after the bus has let go of it.
    _lease: FlashLease<'a>,
}

impl<'a> Bus<'a> {
    fn enable(pins: &'a Pins, camera: &'a Camera, hz: u32) -> Result<Self, String> {
        let driven = driven();
        // On the camera's own supply, the SoC is up too, so it is parked first.
        let lease = camera.lend_flash(!driven.vcc)?;
        let tx = Dma::new(CHUNK)?;
        let rx = Dma::new(CHUNK)?;
        // Power before signals: a pin driven into an unpowered flash feeds it through its
        // input protection.
        let started = take_chip(pins, driven).and_then(|()| start_bus(pins)).and_then(|()| {
            // CS is a GPIO of its own, deselected before it drives, so it can stay low
            // across the pieces of one operation.
            camera::configure(pins.cs, sys::gpio_mode_t_GPIO_MODE_OUTPUT, 1)?;
            add_device(hz)
        });
        match started {
            Ok(device) => {
                let mut bus = Self {
                    pins,
                    device,
                    tx,
                    rx,
                    driven,
                    _lease: lease,
                };
                bus.wait_ready();
                Ok(bus)
            }
            Err(err) => {
                stop_bus(pins);
                release_chip(pins);
                Err(format!("SPI bus: {err}"))
            }
        }
    }

    fn set_hz(&mut self, hz: u32) -> Result<(), EspError> {
        // SAFETY: the device is this bus's and has nothing in flight.
        esp!(unsafe { sys::spi_bus_remove_device(self.device) })?;
        self.device = add_device(hz)?;
        Ok(())
    }

    fn select(&self) {
        set(self.pins.cs, 0);
    }

    fn deselect(&self) {
        set(self.pins.cs, 1);
    }

    /// Waits until the chip has given the same ID for [`STEADY_FOR`], or [`READY_WITHIN`]
    /// passes; a chip that never does is flashrom's to report.
    fn wait_ready(&mut self) {
        let start = Instant::now();
        let mut steady: Option<([u8; 3], Instant)> = None;
        while start.elapsed() < READY_WITHIN {
            self.tx.slice(4).copy_from_slice(&[RDID, 0xFF, 0xFF, 0xFF]);
            self.select();
            let result = self.transfer(4, true);
            self.deselect();
            if result.is_err() {
                return;
            }
            let rx = self.rx.slice(4);
            let id = [rx[1], rx[2], rx[3]];
            let valid = id != [0; 3] && id != [0xFF; 3];
            match steady {
                Some((was, since)) if valid && was == id => {
                    if since.elapsed() >= STEADY_FOR {
                        return;
                    }
                }
                _ => steady = valid.then(|| (id, Instant::now())),
            }
            thread::sleep(Duration::from_millis(1));
        }
        warn!("serprog: no flash answered steadily within {} ms", READY_WITHIN.as_millis());
    }

    /// One SPI transaction of `n` bytes out of `tx`, captured into `rx` when `read`.
    fn transfer(&mut self, n: usize, read: bool) -> Result<(), EspError> {
        // SAFETY: all-zero is a transaction with no flags and no buffers; the ones used are
        // set below.
        let mut transaction: sys::spi_transaction_t = unsafe { core::mem::zeroed() };
        transaction.length = n * 8;
        transaction.__bindgen_anon_1.tx_buffer = self.tx.ptr.cast_const().cast();
        if read {
            transaction.rxlength = n * 8;
            transaction.__bindgen_anon_2.rx_buffer = self.rx.ptr.cast();
        }
        // SAFETY: both buffers are DMA memory of at least `n` bytes that outlive the call,
        // which waits for the transaction to finish.
        esp!(unsafe { sys::spi_device_transmit(self.device, &raw mut transaction) })
    }
}

impl Drop for Bus<'_> {
    fn drop(&mut self) {
        // SAFETY: the device is this bus's and has nothing in flight.
        unsafe { sys::spi_bus_remove_device(self.device) };
        stop_bus(self.pins);
        release_chip(self.pins);
        info!("serprog: clip released and unpowered");
    }
}

/// Powers the chip from its VCC pin, at full strength as it feeds the chip, then drives HOLD
/// high: each only if set to be driven.
fn take_chip(pins: &Pins, driven: Driven) -> Result<(), EspError> {
    if driven.vcc {
        camera::configure(pins.vcc, sys::gpio_mode_t_GPIO_MODE_OUTPUT, 1)?;
        // SAFETY: the pin is configured just above.
        esp!(unsafe { sys::gpio_set_drive_capability(pins.vcc, sys::gpio_drive_cap_t_GPIO_DRIVE_CAP_3) })?;
        thread::sleep(POWER_UP);
    }
    if driven.hold {
        camera::configure(pins.hold, sys::gpio_mode_t_GPIO_MODE_OUTPUT, 1)?;
    }
    Ok(())
}

/// Lets go of HOLD, then of VCC.
fn release_chip(pins: &Pins) {
    for pin in [pins.hold, pins.vcc] {
        if let Err(err) = release(pin) {
            warn!("serprog: GPIO{pin}: {err}");
        }
    }
}

fn start_bus(pins: &Pins) -> Result<(), EspError> {
    // SAFETY: all-zero is valid for this plain C configuration; every field that matters is
    // set below.
    let mut config: sys::spi_bus_config_t = unsafe { core::mem::zeroed() };
    config.__bindgen_anon_1.mosi_io_num = pins.mosi;
    config.__bindgen_anon_2.miso_io_num = pins.miso;
    config.sclk_io_num = pins.clk;
    config.__bindgen_anon_3.quadwp_io_num = -1;
    config.__bindgen_anon_4.quadhd_io_num = -1;
    config.data4_io_num = -1;
    config.data5_io_num = -1;
    config.data6_io_num = -1;
    config.data7_io_num = -1;
    config.max_transfer_sz = i32::try_from(CHUNK).unwrap_or(i32::MAX);
    // SAFETY: `config` outlives the call, which copies it.
    esp!(unsafe { sys::spi_bus_initialize(HOST, &raw const config, sys::spi_common_dma_t_SPI_DMA_CH_AUTO) })
}

/// Frees the bus and leaves every clip pin high-Z, the boot pin included until the camera
/// takes it back.
fn stop_bus(pins: &Pins) {
    // SAFETY: nothing else uses this host; with no bus it only returns an error.
    unsafe { sys::spi_bus_free(HOST) };
    for pin in [pins.cs, pins.clk, pins.miso, pins.mosi] {
        if let Err(err) = release(pin) {
            warn!("serprog: GPIO{pin}: {err}");
        }
    }
}

fn add_device(hz: u32) -> Result<sys::spi_device_handle_t, EspError> {
    // SAFETY: all-zero is mode 0 with the default clock source and duty cycle; the rest is
    // set below.
    let mut config: sys::spi_device_interface_config_t = unsafe { core::mem::zeroed() };
    config.clock_speed_hz = i32::try_from(hz).unwrap_or(i32::MAX);
    config.spics_io_num = -1;
    config.queue_size = 1;
    let mut device = ptr::null_mut();
    // SAFETY: `config` and `device` outlive the call, which copies the configuration.
    esp!(unsafe { sys::spi_bus_add_device(HOST, &raw const config, &raw mut device) })?;
    Ok(device)
}

/// An input with no pulls, driving nothing.
fn release(pin: i32) -> Result<(), EspError> {
    camera::configure(pin, sys::gpio_mode_t_GPIO_MODE_INPUT, 0)
}

fn set(pin: i32, level: u32) {
    // SAFETY: the pin is configured as an output of this module's.
    unsafe { sys::gpio_set_level(pin, level) };
}

/// A buffer the SPI DMA reaches: internal RAM, word-aligned.
struct Dma {
    ptr: *mut u8,
    len: usize,
}

impl Dma {
    fn new(len: usize) -> Result<Self, String> {
        // SAFETY: a plain allocation; the result is checked below.
        let ptr = unsafe { sys::heap_caps_aligned_alloc(4, len, sys::MALLOC_CAP_DMA | sys::MALLOC_CAP_INTERNAL) };
        if ptr.is_null() {
            return Err(format!("no {len} bytes of DMA memory"));
        }
        Ok(Self { ptr: ptr.cast(), len })
    }

    fn slice(&mut self, n: usize) -> &mut [u8] {
        // SAFETY: the buffer is `len` bytes and this struct's alone.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, n.min(self.len)) }
    }
}

impl Drop for Dma {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` and not freed since.
        unsafe { sys::heap_caps_free(self.ptr.cast()) };
    }
}

fn byte(stream: &mut TcpStream) -> io::Result<u8> {
    let mut byte = [0_u8; 1];
    stream.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn u24(bytes: &[u8]) -> usize {
    usize::from(bytes[0]) | usize::from(bytes[1]) << 8 | usize::from(bytes[2]) << 16
}

fn discard(stream: &mut TcpStream, mut count: usize) -> io::Result<()> {
    let mut sink = [0_u8; 256];
    while count > 0 {
        let n = count.min(sink.len());
        stream.read_exact(&mut sink[..n])?;
        count -= n;
    }
    Ok(())
}
