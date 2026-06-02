#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${CARGO_BIN_EXE_dsesh:-"$root/target/debug/dsesh"}"
tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

sock="$tmpdir/session.sock"
first="$tmpdir/first.out"
second="$tmpdir/second.out"

cargo build --quiet --bin dsesh

printf '\034' | "$bin" new "$sock" -- sh -c 'printf "retained-screen-ok\n"; sleep 1' >"$first"

for _ in {1..100}; do
  if [ -S "$sock" ]; then
    break
  fi
  sleep 0.02
done

if [ ! -S "$sock" ]; then
  echo "session socket was not created" >&2
  exit 1
fi

"$bin" attach "$sock" >"$second"

if ! grep -q 'retained-screen-ok' "$second"; then
  echo "reattached client did not receive retained screen contents" >&2
  echo "--- attach output ---" >&2
  sed -n '1,120p' "$second" >&2
  exit 1
fi
