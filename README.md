# thingino backpack

Firmware for an ESP32-S3 (or, untested so far, an ESP32-S2 or ESP32-P4) strapped to one
thingino camera, which makes that camera flashable and debuggable over the network with
nothing else attached:

- **Flashing.** The thingino-dfu daemon (`dfu-remote`) runs on the unit, with the camera's
  USB port on the ESP32-S3's USB host. `thingino-dfu --host` and the web flasher's remote
  mode talk to it as they would to a Linux host: detect, bootstrap, read, write, verify.
- **Serial console.** The camera's UART over TCP, raw and as RFC 2217.
- **Power and boot pin.** The unit switches the camera's power and holds its boot pin, so it
  can put the camera into the bootrom on request and power-cycle one that stopped
  answering on USB.
- **Findable like a camera.** A thingino-style Wi-Fi setup portal on first boot, then mDNS
  `_thingino._tcp` and a status page, which the
  [thingino app](https://github.com/thingino/thingino-app) lists and opens.

Nothing is stored on the unit: the client sends the loader pair with each bootstrap, and
images stream through it, so the default build runs without PSRAM.

## Chips

| Chip     | Build                                   | Network  | Camera USB | Status           |
|----------|-----------------------------------------|----------|------------|------------------|
| ESP32-S3 | `./image.sh`, or `./image.sh psram`     | Wi-Fi    | full speed | in use           |
| ESP32-S2 | `./image.sh s2`                         | Wi-Fi    | full speed | builds, untested |
| ESP32-P4 | `./image.sh p4`                         | Ethernet | high speed | builds, untested |

- **ESP32-S3:** the default build needs no PSRAM; `psram` is for modules with octal PSRAM
  (N8R8, N16R8).
- **ESP32-S2:** one core and 320 KB of SRAM, which does not hold a bootstrap next to Wi-Fi,
  so the build is for modules with PSRAM (quad, like the 2 MB of an S2FN4R2). On boards
  whose only USB port is the native one, that port is the camera's.
- **ESP32-P4:** no radio, so the unit is on Ethernet: the chip's EMAC with an IP101 PHY on
  ESP-IDF's P4 default pins, which are those of Espressif's P4 Function EV board
  (`src/eth.rs`). Addresses come by DHCPv4 and SLAAC, there is no setup portal, and the
  hostname is `thingino-backpack-xxxx` from the Ethernet MAC. The camera's USB goes to the
  high-speed port. The camera's power, boot pin and UART are on GPIO20 to 23, picked
  without a particular board in mind. ESP-IDF 5.5 builds for P4 silicon 3.1 and later;
  `sdkconfig.esp32p4` says what earlier chips need.

## Wiring

| ESP32-S3, -S2   | ESP32-P4                  | Camera                                                                   |
|-----------------|---------------------------|--------------------------------------------------------------------------|
| GPIO15          | GPIO20                    | Power switch (a MOSFET module or similar): high = camera on              |
| GPIO16          | GPIO21                    | Flash DI, pin 5 of an SOIC-8 NOR flash (open-drain: low = boot from USB) |
| GPIO17 (TX)     | GPIO22 (TX)               | UART RX                                                                  |
| GPIO18 (RX)     | GPIO23 (RX)               | UART TX                                                                  |
| GPIO19, GPIO20  | the high-speed port's own | USB D-, D+                                                               |
| GND             | GND                       | GND                                                                      |

- The S2 and S3 share every pin, and GPIO19 and GPIO20 are the only pins their USB PHY
  reaches. The P4's high-speed port has D- and D+ pins of its own, which are not GPIOs.
  The P4's power, boot and UART pins were picked without a particular board in mind: move
  them in `src/main.rs` if the board needs those GPIOs, and keep off its strapping pins,
  GPIO34 to 38.
- The UART is 3.3 V on both sides. The ESP32's TX only drives the camera's RX while the
  camera has power: a driven RX back-powers an unpowered SoC, and some then fail to cold
  boot.
- The boot pin only ever pulls low or lets go, so it never feeds an unpowered camera.
  Pulled low through a power-on, it keeps the bootrom from reading the SPL, and the bootrom
  falls back to USB boot. Its level is set before it becomes an output, so it cannot glitch
  the flash of a running camera when the ESP32 boots.
- The power switch should switch the supply's + side. The camera's ground also reaches the
  ESP32 through the USB cable and the UART ground, so a switch on the ground side can be
  bypassed.
- The ESP32's own log is on UART0 at 115200 8N1: GPIO43 TX and GPIO44 RX on the S2 and S3,
  GPIO37 TX and GPIO38 RX on the P4. Most boards bring it out through their USB-UART
  bridge. The camera's USB port is never a console.
- The pins are set in `src/main.rs`, per chip; the status page lists the ones in use.

## Flashing a release

Each [release](https://github.com/thingino/thingino-backpack/releases) has two images per
chip (`esp32s3`, `esp32s3-psram`, `esp32s2`, `esp32p4`) and a `SHA256SUMS`:

- `thingino-backpack-<chip>.bin` is a first install: the bootloader, the partition table
  and the app in one image, written at 0x0. It erases the saved Wi-Fi, so the unit starts
  in its setup portal.
- `thingino-backpack-<chip>-app.bin` is an update: written at 0x10000, it keeps the Wi-Fi
  settings.

```sh
esptool.py --chip esp32s3 write_flash 0x0 thingino-backpack-esp32s3.bin
esptool.py --chip esp32s3 write_flash 0x10000 thingino-backpack-esp32s3-app.bin
```

The images are built for 4 MB of flash and boot on 4, 8 and 16 MB modules. CI builds all
four on every push and publishes them when a `v*` tag is pushed.

## Building

Requirements:

- The Rust toolchain from [espup](https://github.com/esp-rs/espup) (`channel = "esp"`),
  which has the Xtensa targets of the S2 and S3 and the P4's `riscv32imafc-esp-espidf`,
  plus `ldproxy` and `espflash` (`cargo install ldproxy espflash`).
- ESP-IDF v5.5.5, at `~/esp/esp-idf-v5.5.5` by default (`env.sh` sets `IDF_PATH`).

Cargo fetches the daemon, the DFU core and the ESP-IDF USB host backend from
[thingino-dfu-rs](https://github.com/thingino/thingino-dfu-rs) at its v2.1.1 release.

```sh
./image.sh          # ESP32-S3, images/: bootloader.bin, partition-table.bin, app.bin, full.bin
./image.sh psram    # ESP32-S3 with octal PSRAM, images/psram/
./image.sh s2       # ESP32-S2, images/s2/
./image.sh p4       # ESP32-P4, images/p4/
```

The first build of each compiles ESP-IDF and takes a while; each builds in a target
directory of its own.

`full.bin` is the other three in one image, the one a release calls
`thingino-backpack-<chip>.bin`. Flash it once, with the chip and the directory that match:

```sh
esptool.py --chip esp32s3 write_flash 0x0 images/full.bin
```

After that, `write_flash 0x10000 images/app.bin` updates the firmware and keeps the saved
Wi-Fi settings.

## First boot

On the P4 there is nothing to set up: it takes its addresses on Ethernet. The S2 and S3,
with no saved network, open an access point named `THINGINO-BACKPACK-xxxx` (the
last four hex digits of its MAC) at 172.16.0.1, and answer the thingino cameras' setup
API. Either:

- use the [thingino app](https://github.com/thingino/thingino-app), which finds it by the
  `THINGINO-` prefix and derives the Wi-Fi key on the phone, or
- join the access point and open http://172.16.0.1/ (most phones open it by themselves).

Enter the network, its passphrase, and a hostname (default `thingino-backpack-xxxx`). The
unit restarts and joins. It keeps rejoining after any outage, takes IPv6 addresses by
SLAAC as well as DHCPv4, and announces itself over mDNS.

To set it up again, press **Reset Wi-Fi** on the status page (or `curl -X POST
http://<host>.local/api/wifi-reset`): it forgets the network and the hostname and restarts
into the setup portal. Erasing the NVS partition (`esptool.py erase_region 0x9000 0x6000`)
does the same without a network.

## Using it

| Port      | Service                                                       |
|-----------|---------------------------------------------------------------|
| 80        | Status page and `/api/camera`                                 |
| 2217      | Camera console, RFC 2217                                      |
| 3000      | Camera console, raw                                           |
| 5050      | thingino-dfu daemon (`dfu-remote`)                            |
| 5353/udp  | mDNS: the hostname, and `_thingino._tcp` for the thingino app |

`<host>` below is the hostname, as `<host>.local`, or an address.

### Flashing

```sh
thingino-dfu --host <host>.local:5050 -l          # list what is on the USB port
thingino-dfu --host <host>.local:5050 -b          # bootstrap a camera in the bootrom
thingino-dfu --host <host>.local:5050 -r dump.bin
thingino-dfu --host <host>.local:5050 -w image.bin --verify
```

The unit holds no loaders, so the client sends the pair with each bootstrap: that takes
thingino-dfu 2.1.0 or later, and the web flasher at webflash.thingino.com does it too.
IPv6 addresses go in brackets and quoted: `--host '[2001:db8::1]:5050'`. In the web
flasher, choose remote mode and enter `<host>.local:5050`.

### Console

One client at a time across both console ports; a new connection takes the console over,
and the old client is told who took it. The camera's output while nobody is connected is
dropped.

```sh
socat -,rawer,escape=0x1d tcp:<host>.local:3000     # Ctrl-] leaves
python3 -m serial.tools.miniterm rfc2217://<host>.local:2217 115200
```

RFC 2217 clients can set the baud rate, data bits, parity and stop bits, send a break, and
drive the camera through the modem lines the way esptool's auto-reset circuit drives an
ESP32's EN and IO0:

| DTR | RTS | Camera                   |
|-----|-----|--------------------------|
| on  | off | boot pin held            |
| off | on  | power cut                |
| on  | on  | unchanged                |
| off | off | unchanged                |

Terminals assert both lines when they open, which leaves the camera alone. The lines act
once they have been stable for 100 ms, so a client that sets them one at a time never
pulses the boot pin, and whatever they asserted is undone when the client disconnects.
The UART goes back to 115200 8N1 then too.

### Camera power and boot pin

The status page has buttons for all of these, and the same actions are an HTTP API:

```sh
curl http://<host>.local/api/camera
curl -X POST 'http://<host>.local/api/camera?action=bootrom'
```

| Action         | What it does                                                            |
|----------------|-------------------------------------------------------------------------|
| `power-on`     | Switch the camera on                                                    |
| `power-off`    | Switch the camera off                                                   |
| `power-cycle`  | Off for a second, then on                                               |
| `bootrom`      | Hold the boot pin, power-cycle, release the pin as soon as the bootrom enumerates |
| `boot-hold`    | Hold the boot pin                                                       |
| `boot-release` | Release the boot pin                                                    |

`bootrom` releases the pin as soon as the bootrom shows up, because U-Boot needs the flash
afterwards. It reports failure, and releases the pin, when nothing enumerates within 10
s, or when the camera was still on the USB bus after its power was cut: then the power
switch is not doing its job. A `bootrom` followed by `thingino-dfu -b` puts a camera into
the DFU gadget without anyone touching it.

Answers are JSON: `{"ok":true,"message":...}` or `{"ok":false,"error":...}`. `GET
/api/camera` reports the power, the boot pin, the USB devices enumerated, how long a USB
transfer has gone unanswered, and the power cycles done for recovery.

### Recovery

A camera that stops serving USB without leaving the bus leaves a control transfer
unanswered for good, and the ESP-IDF USB host has no transfer timeout. When that lasts 10
s, or when three port power cycles in a row fail to get the camera enumerated, the unit
power-cycles the camera. It backs off from 30 s to 10 min while that keeps happening, and
leaves a camera alone while its power is meant to be off or a bootrom entry is running.

## Memory

The default build runs without PSRAM, which is what the stack sizes and the streaming are
shaped for. The log prints the internal heap every 10 s (free, lowest ever, largest block)
and every task's unused stack every minute. On a T31X with a console client attached, the
lowest free heap is about 100 KB, reached during a bootstrap, and about 120 KB during reads
and writes.

## Security

Nothing on the unit asks for credentials: anyone who can reach it can flash the camera,
type into its console, and cut its power. Keep it on a network you trust.

## Troubleshooting

- **Brownouts at Wi-Fi start-up** (`BOD` in the log, reset loops): the radio's calibration
  is the current peak. After a brownout reset the radio comes up at 13 dBm instead of 20,
  until a reset for any other reason; a camera and the ESP32 on one weak USB port need a
  powered hub.
- **RFC 2217 or the raw console stalls over IPv4 while IPv6 works**: some access points'
  hardware receive offload turns the Ethernet padding of tiny frames (1 to 5 bytes of TCP
  payload, as keystrokes are) into payload, and the connection desyncs. OpenWrt's airoha
  hardware GRO does this; turning it off (`ethtool -K <if> rx-gro-hw off`) fixes it.
