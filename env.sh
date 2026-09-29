# Source before cargo: the ESP-IDF tree, and the clang that bindgen needs (espup's
# export-esp.sh). Either can come from the environment instead, as in CI.
export IDF_PATH="${IDF_PATH:-$HOME/esp/esp-idf-v5.5.5}"
[ -n "$LIBCLANG_PATH" ] || . "$HOME/export-esp.sh"
