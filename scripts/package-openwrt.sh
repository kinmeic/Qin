#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
  echo "usage: package-openwrt.sh <binary> <openwrt-architecture> <version> <output-directory>" >&2
  exit 2
fi

binary=$1
architecture=$2
version=$3
output_dir=$4

test -f "$binary"
case "$architecture" in
  aarch64_cortex-a53|x86_64) ;;
  *) echo "unsupported OpenWrt architecture: $architecture" >&2; exit 2 ;;
esac

work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM
mkdir -p "$work_dir/control" "$work_dir/data/usr/bin" "$work_dir/data/etc/qin" "$output_dir"
output_dir=$(cd "$output_dir" && pwd)

cp "$binary" "$work_dir/data/usr/bin/qin"
chmod 0755 "$work_dir/data/usr/bin/qin"
cp packaging/openwrt/files/config.toml.example "$work_dir/data/etc/qin/config.toml.example"
chmod 0600 "$work_dir/data/etc/qin/config.toml.example"

cat > "$work_dir/control/control" <<EOF
Package: qin
Version: ${version}-1
Architecture: ${architecture}
Maintainer: Qin contributors
Section: utils
Priority: optional
Depends: ca-bundle
Description: Natural-language command-line AI agent with sessions and knowledge search.
EOF

printf '2.0\n' > "$work_dir/debian-binary"
tar -C "$work_dir/control" --sort=name --owner=0 --group=0 --numeric-owner --mtime='@0' -czf "$work_dir/control.tar.gz" .
tar -C "$work_dir/data" --sort=name --owner=0 --group=0 --numeric-owner --mtime='@0' -czf "$work_dir/data.tar.gz" .

ipk=$output_dir/qin_${version}-1_${architecture}.ipk
(cd "$work_dir" && ar r "$ipk" debian-binary control.tar.gz data.tar.gz)
