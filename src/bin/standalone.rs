//! Standalone check: thingino-dfu's core driving a camera from the ESP32-S3 itself. Waits
//! for an Ingenic device on the OTG port, bootstraps it if it is in the bootrom, then reads
//! the whole flash over DFU and logs its sha256. No network is involved.

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use std::io::Write;
use std::time::Instant;

use esp_idf_svc::sys;
use log::{error, info, warn};
use sha2::{Digest, Sha256};
#[cfg(feature = "verify-uboot")]
use tdfu_core::addr::Kseg1;
#[cfg(feature = "verify-uboot")]
use tdfu_core::bootrom;
use tdfu_core::clock::BlockingClock;
use tdfu_core::ops::{self, Stage};
#[cfg(feature = "verify-uboot")]
use tdfu_core::Phase;
use tdfu_core::{AltSel, Progress};
use tdfu_usb::{vid, ControlIn, ControlType, Discovered, LocalUsbBackend, LocalUsbTransport, Recipient};

#[cfg(feature = "verify-uboot")]
use thingino_backpack::usbhost::EspTransport;
use thingino_backpack::usbhost::UsbHost;

static STAGE1: &[u8] = include_bytes!("../../loaders/t31x/tpl.bin");
static UBOOT: &[u8] = include_bytes!("../../loaders/t31x/uboot.bin");

/// The test camera's flash: the random image written to it over USB/IP on 2026-09-28.
const EXPECTED_SHA256: &str = "a1ea2d5956e6d5c0772c9aacab10d56002b1ba2c910120a177861bff31dc47d2";
const GADGET_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(feature = "verify-uboot")]
const READBACK_CHUNK: usize = 16 * 1024;

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    // The operations are deep async state machines; they get a large stack of their own.
    let job = std::thread::Builder::new()
        .name("dfu".into())
        .stack_size(64 * 1024)
        .spawn(serve)
        .expect("spawning the dfu task");
    if let Err(err) = job.join().unwrap_or_else(|_| Err("the dfu task panicked".into())) {
        error!("{err}");
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

/// One run per camera: replugging it (or power-cycling it) starts the next.
fn serve() -> Result<(), String> {
    let host = UsbHost::install().map_err(|err| err.to_string())?;
    heap("start");
    loop {
        match attempt(&host) {
            Ok(()) => info!("done"),
            Err(err) => error!("failed: {err}"),
        }
        info!("waiting for the camera to leave the bus; replug it to run again");
        while block_on(host.list()).map_err(|err| err.to_string())?.iter().any(|dev| vid::is_ingenic(dev.descriptors.vendor_id)) {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

fn attempt(host: &UsbHost) -> Result<(), String> {
    let clock = BlockingClock;
    info!("waiting for an Ingenic device on the OTG port");
    let (mut id, stage) = wait_for(host, None, None)?;
    if stage == Stage::Bootrom {
        let dev = block_on(host.open(&id)).map_err(|err| err.to_string())?;
        let started = Instant::now();
        let detection = block_on(ops::detect(&dev, &clock)).map_err(|err| err.to_string())?;
        info!("detected in {:?}: {detection:?}", started.elapsed());
        let started = Instant::now();
        #[cfg(feature = "verify-uboot")]
        bootstrap_verified(&dev, &clock)?;
        #[cfg(not(feature = "verify-uboot"))]
        block_on(ops::bootstrap(&dev, &clock, STAGE1, UBOOT, &mut progress_logger()))
            .map_err(|err| err.to_string())?;
        drop(dev);
        let sent = started.elapsed();
        match wait_for(host, Some(Stage::Gadget), Some(GADGET_TIMEOUT)) {
            Ok((gadget, _)) => id = gadget,
            Err(err) => {
                still_answering(host, id);
                return Err(err);
            }
        }
        info!("bootstrap: loaders sent in {sent:?}, gadget up {:?} after the start", started.elapsed());
    }

    let dev = block_on(host.open(&id)).map_err(|err| err.to_string())?;
    let dfu = block_on(ops::probe(&dev, &clock)).map_err(|err| err.to_string())?;
    info!("{dfu:?}");
    heap("before read");
    let mut sink = HashSink::default();
    let started = Instant::now();
    let bytes = block_on(ops::read(&dev, &clock, &AltSel::Default, None, &mut sink, &mut progress_logger()))
        .map_err(|err| err.to_string())?;
    let secs = started.elapsed().as_secs_f64();
    let digest: String = sink.hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect();
    info!("read {bytes} bytes in {secs:.1} s ({:.0} KB/s), sha256 {digest}", bytes as f64 / secs / 1000.0);
    if digest == EXPECTED_SHA256 {
        info!("sha256 MATCHES the image written over USB/IP");
    } else {
        warn!("sha256 does NOT match {EXPECTED_SHA256}");
    }
    heap("end");
    Ok(())
}

#[cfg(feature = "verify-uboot")]
/// `ops::bootstrap`, with U-Boot read back out of DDR between the cache flush and the
/// jump. The readback goes through the uncached kseg1 alias and only after `FLUSH_CACHE`:
/// before it, the image can still sit in the D-cache and DDR would compare stale.
fn bootstrap_verified(dev: &EspTransport, clock: &BlockingClock) -> Result<(), String> {
    let err = |err: tdfu_core::Error| err.to_string();
    let stage1 = bootrom::pad_stage1(STAGE1);
    let uboot = bootrom::pad_stage1(UBOOT);
    let mut progress = progress_logger();
    progress(Progress::Phase(Phase::Stage1));
    block_on(bootrom::load_to_memory(dev, clock, bootrom::SPL_LOAD_ADDR, &stage1, &mut progress)).map_err(err)?;
    block_on(bootrom::prog_stage1(dev, clock, bootrom::SPL_ENTRY_ADDR)).map_err(err)?;
    std::thread::sleep(ops::POST_STAGE1_SETTLE);
    progress(Progress::Phase(Phase::UBoot));
    block_on(bootrom::load_to_memory(dev, clock, bootrom::UBOOT_ADDR, &uboot, &mut progress)).map_err(err)?;
    block_on(bootrom::flush_cache(dev, clock)).map_err(err)?;

    let started = Instant::now();
    block_on(bootrom::claim(dev)).map_err(err)?;
    let mut mismatch = None;
    for (index, expected) in uboot.chunks(READBACK_CHUNK).enumerate() {
        let offset = index * READBACK_CHUNK;
        let addr = Kseg1::from_phys(bootrom::UBOOT_ADDR + offset as u32);
        let got = block_on(bootrom::read_memory(dev, clock, addr, expected.len())).map_err(err)?;
        if let Some(at) = got.iter().zip(expected).position(|(got, want)| got != want) {
            let bad = got.iter().zip(expected).filter(|(got, want)| got != want).count();
            let end = (at + 16).min(expected.len());
            mismatch = Some(format!(
                "U-Boot differs in DDR at +{:#x} ({bad} of {} bytes in that chunk): got {:02x?}, want {:02x?}",
                offset + at,
                expected.len(),
                &got[at..end],
                &expected[at..end]
            ));
            break;
        }
    }
    block_on(bootrom::release(dev)).map_err(err)?;
    if let Some(mismatch) = mismatch {
        return Err(mismatch);
    }
    info!("U-Boot verified in DDR: {} bytes read back in {:?}", uboot.len(), started.elapsed());

    block_on(bootrom::prog_stage2(dev, clock, bootrom::UBOOT_ADDR)).map_err(err)?;
    info!("U-Boot starting; the device will re-enumerate in DFU mode");
    Ok(())
}

/// After a bootstrap with no gadget: does the old device still answer on EP0? An answer
/// means the bootrom never jumped; silence means the SoC jumped and stopped serving USB
/// without dropping off the bus.
fn still_answering(host: &UsbHost, id: u8) {
    let listed = block_on(host.list()).unwrap_or_default();
    if !listed.iter().any(|dev| dev.id == id) {
        info!("address {id} is no longer listed");
        return;
    }
    let Ok(dev) = block_on(host.open(&id)) else {
        info!("address {id} is listed but cannot be opened");
        return;
    };
    let request = ControlIn {
        control_type: ControlType::Standard,
        recipient: Recipient::Device,
        request: 0x06,
        value: 0x0100,
        index: 0,
        len: 18,
    };
    match block_on(dev.control_in(request, Duration::from_secs(1))) {
        Ok(desc) => info!("address {id} still answers GET_DESCRIPTOR ({} bytes): the bootrom never jumped", desc.len()),
        Err(err) => info!("address {id} does not answer ({err}): the SoC jumped and hung with USB still attached"),
    }
}

/// Polls until an Ingenic device in `want` (any stage when `None`) is on the bus.
fn wait_for(host: &UsbHost, want: Option<Stage>, timeout: Option<Duration>) -> Result<(u8, Stage), String> {
    let deadline = timeout.map(|timeout| Instant::now() + timeout);
    loop {
        for Discovered { id, descriptors } in block_on(host.list()).map_err(|err| err.to_string())? {
            if !vid::is_ingenic(descriptors.vendor_id) {
                continue;
            }
            let Some(stage) = ops::classify(&descriptors) else {
                continue;
            };
            if want.is_none_or(|want| want == stage) {
                info!(
                    "{:04x}:{:04x} {:?} at address {id}: {stage}",
                    descriptors.vendor_id,
                    descriptors.product_id,
                    descriptors.product_string.as_deref().unwrap_or("")
                );
                return Ok((id, stage));
            }
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(format!("no {want:?} device within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn progress_logger() -> impl FnMut(Progress) {
    let mut decile = 0;
    move |progress| match progress {
        Progress::Phase(phase) => {
            decile = 0;
            info!("phase {phase:?}");
        }
        Progress::Bytes {
            phase,
            done,
            total: Some(total),
        } if total > 0 && done * 10 / total > decile => {
            decile = done * 10 / total;
            info!("{phase:?} {}%  {done}/{total}", decile * 10);
        }
        Progress::Note(note) => info!("{note}"),
        _ => {}
    }
}

#[derive(Default)]
struct HashSink {
    hasher: Sha256,
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hasher.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn heap(when: &str) {
    let free = unsafe { sys::esp_get_free_heap_size() };
    let dma = unsafe { sys::heap_caps_get_largest_free_block(sys::MALLOC_CAP_DMA) };
    info!("heap {when}: {free} bytes free, largest DMA block {dma}");
}

/// Every await here completes synchronously (the backend blocks), so one poll finishes the
/// future; the loop only covers an unexpected `Pending`.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        std::thread::yield_now();
    }
}
