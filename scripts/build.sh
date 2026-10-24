#!/bin/sh
# Produces bin/herdr-pm for the plugin. Herdr runs this on install; run it
# yourself after `herdr plugin link` during development.
#
# A prebuilt bin/herdr-pm whose version matches herdr-plugin.toml is kept as
# is, so a branch that ships binaries installs without a Rust toolchain.
# HERDR_PM_BUILD_FROM_SOURCE=1 forces a build.
set -eu
cd "$(dirname "$0")/.."

want="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' herdr-plugin.toml | head -n 1)"
mkdir -p bin

if [ "${HERDR_PM_BUILD_FROM_SOURCE:-0}" != "1" ] && [ -x bin/herdr-pm ]; then
  have="$(./bin/herdr-pm --version 2>/dev/null | awk '{print $2}')" || have=""
  if [ "$have" = "$want" ]; then
    echo "herdr-plugin-manager: using prebuilt bin/herdr-pm $have"
    exit 0
  fi
fi

if ! command -v cargo >/dev/null 2>&1; then
  echo "herdr-plugin-manager: building needs Rust (https://rustup.rs) and a C compiler," >&2
  echo "or a checkout that already contains bin/herdr-pm $want." >&2
  exit 1
fi

echo "herdr-plugin-manager: building $want with $(cargo --version)"
cargo build --release --locked --quiet
cp target/release/herdr-pm bin/herdr-pm.tmp
mv -f bin/herdr-pm.tmp bin/herdr-pm
echo "herdr-plugin-manager: built bin/herdr-pm"
