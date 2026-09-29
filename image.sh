#!/bin/sh
# Builds the three flash images: bootloader (0x0), partition table (0x8000), app (0x10000).
# Flashing them separately keeps whatever NVS the board already holds.
#
# `./image.sh <variant>` layers sdkconfig.<variant> over sdkconfig.defaults and builds in a
# target directory of its own, so switching variants does not rebuild ESP-IDF each time:
# `./image.sh nopsram` is for modules without PSRAM.
set -e
cd "$(dirname "$0")"
. ./env.sh
images=images
if [ -n "$1" ]; then
	export CARGO_TARGET_DIR="target-$1"
	export ESP_IDF_SDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.$1"
	images="images/$1"
fi
cargo build --release --bin backpack
out=${CARGO_TARGET_DIR:-target}/xtensa-esp32s3-espidf/release
mkdir -p "$images"
cp "$(ls -t "$out"/build/esp-idf-sys-*/out/build/bootloader/bootloader.bin | head -1)" "$images"/bootloader.bin
"$HOME"/.espressif/python_env/idf5.5_py3.13_env/bin/python \
	"$IDF_PATH/components/partition_table/gen_esp32part.py" partitions.csv "$images"/partition-table.bin >/dev/null
espflash save-image --chip esp32s3 --flash-size 8mb "$out/backpack" "$images"/app.bin
ls -l "$images"
