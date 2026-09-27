#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

HOST_TRIPLE="$(rustc +stable -vV | sed -n 's/^host: //p')"
if [[ -z "$HOST_TRIPLE" ]]; then
  echo 'host-test-native-target-isolation=failed error=unable-to-determine-stable-rust-host-target' >&2
  exit 1
fi

printf 'host-test-native-target=%s\n' "$HOST_TRIPLE"
cargo +stable test --target "$HOST_TRIPLE" --lib
echo 'host-test-native-target-isolation=ok'

export PYTHONDONTWRITEBYTECODE=1
python3 -B -m unittest discover -s tools/lexicon/tests
# build.sh reaches this check through validate.sh. ESP-IDF and other generated
# trees (notably .embuild) contain Python bytecode that is not tracked source.
if find . \
  \( -name .git -o -name target -o -name .embuild -o -name dist \) -prune \
  -o -type d -name '__pycache__' -print -quit | grep -q .; then
  echo 'python-cache=failed' >&2
  exit 1
fi
echo 'lexicon-converter-tests=ok'
