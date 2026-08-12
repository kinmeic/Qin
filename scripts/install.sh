#!/bin/sh
set -eu

prefix=${PREFIX:-/usr/local}
source_bin=${1:-target/release/qin}

if [ ! -f "$source_bin" ]; then
  echo "qin binary not found: $source_bin" >&2
  echo "run: cargo build --release" >&2
  exit 1
fi

install -d "$prefix/bin"
install -m 0755 "$source_bin" "$prefix/bin/qin"
echo "installed: $prefix/bin/qin"
echo "next: qin init"

