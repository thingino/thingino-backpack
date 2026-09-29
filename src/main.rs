//! Backpack spike: thingino-dfu's core driving a camera from the ESP32-S3 itself. Waits
//! for an Ingenic device on the OTG port, bootstraps it if it is in the bootrom, then reads
//! the whole flash over DFU and logs its sha256. No network is involved.

mod usbhost;

use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use core::time::Duration;
use std::io::Write;
use std::time::Instant;

use esp_idf_svc::sys;
use log::{error, info, warn};
use sha2::{Digest, Sha256};
use tdfu_core::clock::BlockingClock;
use tdfu_core::ops::{self, Stage};
use tdfu_core::{AltSel, Progress};
use tdfu_usb::{vid, Discovered, LocalUsbBackend};

use crate::usbhost::UsbHost;

static STAGE1: &[u8] = include_bytes!("../loaders/t31x/tpl.bin");
static UBOOT: &[u8] = include_bytes!("../loaders/t31x/uboot.bin");

/// The test camera's flash: the random image written to it over USB/IP on 2026-09-28.
const EXPECTED_SHA256: &str = "a1ea2d5956e6d5c0772c9aacab10d56002b1ba2c910120a177861bff31dc47d2";
const GADGET_TIMEOUT: Duration = Duration::from_secs(30);

fn main() {
    sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    // The operations are deep async state machines; they get a large stack of their own.
    let job = std::thread::Builder::new()
        .name("dfu".into())
        .stack_size(64 * 1024)
        .spawn(run)
        .expect("spawning the dfu task");
    match job.join() {
        Ok(Ok(())) => info!("done"),
        Ok(Err(err)) => error!("failed: {err}"),
        Err(_) => error!("the dfu task panicked"),
    }
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn run() -> Result<(), String> {
    let host = UsbHost::install().map_err(|err| err.to_string())?;
    let clock = BlockingClock;
    heap("start");

    info!("waiting for an Ingenic device on the OTG port");
    let (mut id, stage) = wait_for(&host, None, None)?;
    if stage == Stage::Bootrom {
        let dev = block_on(host.open(&id)).map_err(|err| err.to_string())?;
        let started = Instant::now();
        let detection = block_on(ops::detect(&dev, &clock)).map_err(|err| err.to_string())?;
        info!("detected in {:?}: {detection:?}", started.elapsed());
        let started = Instant::now();
        block_on(ops::bootstrap(&dev, &clock, STAGE1, UBOOT, &mut progress_logger()))
            .map_err(|err| err.to_string())?;
        drop(dev);
        let sent = started.elapsed();
        (id, _) = wait_for(&host, Some(Stage::Gadget), Some(GADGET_TIMEOUT))?;
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
