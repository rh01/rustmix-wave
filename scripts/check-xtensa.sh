#!/usr/bin/env bash
# Type-check espidf-only modules. Host tests never compile
# `#[cfg(target_os = "espidf")]` code, which is where the ESP-IDF 5.4.3
# `c_char` / `Option<AuthMethod>` mismatches show up.
#
# Requires the `esp` Rust toolchain from rust-toolchain.toml, `ldproxy`, and
# ESP-IDF 5.4.3 (embuild downloads it on first run). GitHub Actions does not
# install that toolchain, so this stays a local/script check.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f "${HOME}/export-esp.sh" ]]; then
  # shellcheck disable=SC1091
  source "${HOME}/export-esp.sh"
fi

cargo check --target xtensa-esp32s3-espidf
echo 'xtensa-espidf-check=ok'
