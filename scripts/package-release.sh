#!/bin/sh
set -eu

if [ "$#" -ne 3 ]; then
  echo "usage: package-release.sh <binary> <asset-name> <output-directory>" >&2
  exit 2
fi

binary=$1
asset_name=$2
output_dir=$3

test -f "$binary"
work_dir=$(mktemp -d)
trap 'rm -rf "$work_dir"' EXIT HUP INT TERM

package_dir=$work_dir/$asset_name
mkdir -p "$package_dir" "$output_dir"
cp "$binary" "$package_dir/qin"
chmod 0755 "$package_dir/qin"
cp README.md LICENSE SECURITY.md "$package_dir/"
cp assets/config.example.toml "$package_dir/config.toml.example"

tar -C "$work_dir" -czf "$output_dir/$asset_name.tar.gz" "$asset_name"
