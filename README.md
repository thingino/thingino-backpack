# thingino backpack

Firmware for an ESP32-S3 strapped to one thingino camera, which makes that camera
flashable and debuggable over the network with nothing else attached:

- **Flashing.** The thingino-dfu daemon (`dfu-remote`) runs on the unit, with the camera's
  USB port on the ESP32-S3's USB host. `thingino-dfu --host` and the web flasher's remote
  mode talk to it as they would to a Linux host: detect, bootstrap, read, write, verify.
- **Serial console.** The camera's UART over TCP, raw and as RFC 2217.
- **Power and boot pin.** The unit switches the camera's power and holds its boot pin, so it
  can put the camera into the bootrom on request and power-cycle one that stopped
  answering on USB.
- **Findable like a camera.** A thingino-style Wi-Fi setup portal on first boot, then mDNS
  `_thingino._tcp` and a status page, which the thingino app lists and opens.

Nothing is stored on the unit: the client sends the loader pair with each bootstrap, and
images stream through it, so the default build runs without PSRAM.

## Wiring

| ESP32-S3        | Camera                                                                    |
|-----------------|---------------------------------------------------------------------------|
| GPIO15          | Power switch (a MOSFET module or similar): high = camera on               |
| GPIO16          | Flash DI, pin 5 of an SOIC-8 NOR flash (open-drain: low = boot from USB)  |
| GPIO17 (TX)     | UART RX                                                                   |
| GPIO18 (RX)     | UART TX                                                                   |
| GPIO19, GPIO20  | USB D-, D+ (the ESP32-S3's OTG port)                                      |
| GND             | GND                                                                       |

- The UART is 3.3 V on both sides. GPIO17 only drives the camera's RX while the camera has
  power: a driven RX back-powers an unpowered SoC, and some then fail to cold boot.
- GPIO16 only ever pulls low or lets go, so it never feeds an unpowered camera. Pulled low
  through a power-on, it keeps the bootrom from reading the SPL, and the bootrom falls back
  to USB boot. Its level is set before it becomes an output, so it cannot glitch the flash
  of a running camera when the ESP32 boots.
- The power switch should switch the supply's + side. The camera's ground also reaches the
  ESP32 through the USB cable and the UART ground, so a switch on the ground side can be
  bypassed.
- The ESP32's own log is on UART0 (GPIO43 TX, GPIO44 RX), 115200 8N1, which is the port
  most boards bring out through their USB-UART bridge. The OTG port belongs to the camera.
- The pins are set in `src/main.rs`; the status page lists the ones in use.

## Building

Requirements:

- The Xtensa Rust toolchain from [espup](https://github.com/esp-rs/espup) (`channel =
  "esp"`), plus `ldproxy` and `espflash` (`cargo install ldproxy espflash`).
- ESP-IDF v5.5.5, at `~/esp/esp-idf-v5.5.5` by default (`env.sh` sets `IDF_PATH`).
- [thingino-dfu-rs](https://github.com/thingino/thingino-dfu-rs) checked out next to this
  repository as `thingino-dfu-rs-espidf`, on its `espidf-backend` branch, which holds the
  ESP-IDF USB host backend until it is merged.

```sh
./image.sh          # images/: bootloader.bin, partition-table.bin, app.bin
./image.sh psram    # images/psram/: for modules with octal PSRAM (N8R8, N16R8)
```

The first build compiles ESP-IDF and takes a while; each variant builds in a target
directory of its own.

Flash all three images once:

```sh
esptool.py --chip esp32s3 write_flash \
  0x0 images/bootloader.bin 0x8000 images/partition-table.bin 0x10000 images/app.bin
```

After that, `write_flash 0x10000 images/app.bin` updates the firmware and keeps the saved
Wi-Fi settings. The image is built for 4 MB of flash and boots on 4, 8 and 16 MB modules.

## First boot

With no saved network the unit opens an access point named `THINGINO-BACKPACK-xxxx` (the
last four hex digits of its MAC) at 172.16.0.1, and answers the thingino cameras' setup
API. Either:

- use the thingino app, which finds it by the `THINGINO-` prefix and derives the Wi-Fi key
  on the phone, or
- join the access point and open http://172.16.0.1/ (most phones open it by themselves).

Enter the network, its passphrase, and a hostname (default `thingino-backpack-xxxx`). The
unit restarts and joins. It keeps rejoining after any outage, takes IPv6 addresses by
SLAAC as well as DHCPv4, and announces itself over mDNS. To set it up again, erase the NVS
partition (`esptool.py erase_region 0x9000 0x6000`).

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
lowest free heap is about 65 KB, reached during a bootstrap, and about 120 KB during reads
and writes.

## Security

Nothing on the unit asks for credentials: anyone who can reach it can flash the camera,
type into its console, and cut its power. Keep it on a network you trust.

## Troubleshooting

- **Brownouts at Wi-Fi start-up** (`BOD` in the log, reset loops): the radio's calibration
  is the current peak. The TX power is capped at 13 dBm; a camera and the ESP32 on one weak
  USB port need a powered hub.
- **RFC 2217 or the raw console stalls over IPv4 while IPv6 works**: some access points'
  hardware receive offload turns the Ethernet padding of tiny frames (1 to 5 bytes of TCP
  payload, as keystrokes are) into payload, and the connection desyncs. OpenWrt's airoha
  hardware GRO does this; turning it off (`ethtool -K <if> rx-gro-hw off`) fixes it.
