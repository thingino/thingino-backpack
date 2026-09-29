#!/bin/sh
# Builds the three flash images: bootloader (0x0), partition table (0x8000), app (0x10000).
# Flashing them separately keeps whatever NVS the board already holds.
#
# `./image.sh` builds for the ESP32-S3 without PSRAM. A variant builds in a target directory
# of its own, so switching does not rebuild ESP-IDF each time:
#   psram   ESP32-S3 with octal PSRAM (sdkconfig.psram)
#   s2      ESP32-S2 with PSRAM (sdkconfig.esp32s2)
#   p4      ESP32-P4 on Ethernet (sdkconfig.esp32p4)
set -e
cd "$(dirname "$0")"
. ./env.sh
chip=esp32s3
target=xtensa-esp32s3-espidf
images=images
case "$1" in
"") ;;
psram) overlay=sdkconfig.psram ;;
s2) chip=esp32s2 target=xtensa-esp32s2-espidf overlay=sdkconfig.esp32s2 ;;
p4) chip=esp32p4 target=riscv32imafc-esp-espidf overlay=sdkconfig.esp32p4 ;;
*) echo "usage: $0 [psram|s2|p4]" >&2; exit 2 ;;
esac
if [ -n "$1" ]; then
	export CARGO_TARGET_DIR="target-$1"
	export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;$overlay"
	images="images/$1"
fi
export MCU="$chip"
cargo build --release --bin backpack --target "$target"
out=${CARGO_TARGET_DIR:-target}/$target/release
mkdir -p "$images"
cp "$(ls -t "$out"/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin | head -1)" "$images"/bootloader.bin
python3 "$IDF_PATH/components/partition_table/gen_esp32part.py" partitions.csv "$images"/partition-table.bin >/dev/null
espflash save-image --chip "$chip" --flash-size 4mb "$out/backpack" "$images"/app.bin
ls -l "$images"
