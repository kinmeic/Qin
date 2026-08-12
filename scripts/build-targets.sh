#!/bin/sh
set -eu

targets=${*:-"x86_64-unknown-linux-musl aarch64-unknown-linux-musl"}
for target in $targets; do
  echo "building $target"
  cargo build --release --target "$target"
done

