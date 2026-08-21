#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "$0")/.." && pwd)"
workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

cp "$repo_root/fixtures/replay/release-config.toml" "$workdir/config.toml"
# Keep the lightweight session store inside the throwaway workdir; otherwise
# the replay would replace the user's real default session file.
printf '\ndata_dir = "%s"\n' "$workdir" >>"$workdir/config.toml"
chmod 600 "$workdir/config.toml"
cargo build --release --manifest-path "$repo_root/Cargo.toml" >/dev/null

output="$(
    cd "$workdir"
    "$repo_root/target/release/qin" \
        --config "$workdir/config.toml" \
        --yes \
        --quiet \
        replay "$repo_root/fixtures/replay/basic.jsonl"
)"

if [[ "$output" != *"Replay complete."* ]]; then
    printf 'release replay smoke test failed: %s\n' "$output" >&2
    exit 1
fi

printf 'release replay smoke test passed\n'
