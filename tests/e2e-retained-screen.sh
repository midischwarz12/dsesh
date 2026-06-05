#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 midischwarz12
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bin="${CARGO_BIN_EXE_dsesh:-"$root/target/debug/dsesh"}"
tmpdir="$(mktemp -d)"
cleanup() {
  set +e
  pkill -f "$tmpdir" >/dev/null 2>&1
  rm -rf "$tmpdir"
}
trap cleanup EXIT

sock="$tmpdir/session.sock"
sig_sock="$tmpdir/signalled.sock"
thread_sock="$tmpdir/thread-regression.sock"
alt_sock="$tmpdir/alternate-screen.sock"
cwd_sock="$tmpdir/cwd-session.sock"
first="$tmpdir/first.out"
second="$tmpdir/second.out"
signalled="$tmpdir/signalled.out"
thread_out="$tmpdir/thread-regression.out"
alt_first="$tmpdir/alternate-first.out"
alt_second="$tmpdir/alternate-second.out"
alt_ready="$tmpdir/alternate-ready"
cwd_dir="$tmpdir/cwd-target"
cwd_out="$tmpdir/cwd.out"
cwd_marker="$tmpdir/cwd-marker"

mkdir -p "$cwd_dir"

cargo build --quiet --bin dsesh

printf '\034' | "$bin" new "$sock" -- sh -c 'printf "retained-screen-ok\n"; sleep 1' >"$first"

if ! grep -q '\[detached\]' "$first"; then
  echo "detached client did not print detach marker" >&2
  echo "--- detach output ---" >&2
  sed -n '1,120p' "$first" >&2
  exit 1
fi

if ! grep -Fq "[detached] $sock" "$first"; then
  echo "detached client did not print socket path" >&2
  echo "--- detach output ---" >&2
  sed -n '1,120p' "$first" >&2
  exit 1
fi

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

"$bin" run "$sock" >"$second"

if ! grep -q 'retained-screen-ok' "$second"; then
  echo "reattached client did not receive retained screen contents" >&2
  echo "--- attach output ---" >&2
  sed -n '1,120p' "$second" >&2
  exit 1
fi

if ! grep -q '\[EOF - ended session\]' "$second"; then
  echo "ended session did not print EOF marker" >&2
  echo "--- attach output ---" >&2
  sed -n '1,120p' "$second" >&2
  exit 1
fi

(
  cd "$cwd_dir"
  {
    for _ in {1..100}; do
      if [ -e "$cwd_marker" ]; then
        break
      fi
      sleep 0.02
    done
    printf '\034'
  } | "$bin" new "$cwd_sock" -- sh -c 'printf "cwd:%s\n" "$PWD" > "$1"; sleep 1' sh "$cwd_marker"
) >"$cwd_out"

if ! grep -q "cwd:$cwd_dir" "$cwd_marker"; then
  echo "session command did not inherit dsesh invocation cwd" >&2
  echo "--- cwd output ---" >&2
  sed -n '1,120p' "$cwd_out" >&2
  echo "--- cwd marker ---" >&2
  sed -n '1,120p' "$cwd_marker" >&2 || true
  exit 1
fi

set +e
printf '\003' | "$bin" new "$sig_sock" -- sh -c 'sleep 10' >"$signalled"
signalled_status=$?
set -e

if [ "$signalled_status" -eq 0 ]; then
  echo "ctrl-c test command unexpectedly exited successfully" >&2
  exit 1
fi

if ! grep -q '\[EOF - ended session\]' "$signalled"; then
  echo "signalled session did not print EOF marker" >&2
  echo "--- signalled output ---" >&2
  sed -n '1,120p' "$signalled" >&2
  exit 1
fi

printf '\034' | "$bin" new "$thread_sock" -- sh -c 'sleep 10' >"$thread_out"

for _ in {1..100}; do
  if [ -S "$thread_sock" ]; then
    break
  fi
  sleep 0.02
done

if [ ! -S "$thread_sock" ]; then
  echo "thread regression session socket was not created" >&2
  exit 1
fi

server_pid=""
for _ in {1..100}; do
  server_pid="$(pgrep -f "dsesh.*server $thread_sock" | head -n1 || true)"
  if [ -n "$server_pid" ]; then
    break
  fi
  sleep 0.02
done

if [ -z "$server_pid" ]; then
  echo "could not find dsesh server process for thread regression test" >&2
  exit 1
fi

for _ in {1..50}; do
  printf '\034' | "$bin" run "$thread_sock" >/dev/null
done

sleep 0.2
threads="$(awk '/^Threads:/ {print $2}' "/proc/$server_pid/status")"

if [ "$threads" -gt 4 ]; then
  echo "quiet detach leaked server threads: $threads" >&2
  ps -L -p "$server_pid" -o pid,tid,stat,comm >&2 || true
  exit 1
fi

printf '\003' | "$bin" run "$thread_sock" >/dev/null || true

{
  for _ in {1..100}; do
    if [ -e "$alt_ready" ]; then
      break
    fi
    sleep 0.02
  done
  printf '\034'
} | "$bin" --rows 24 --cols 80 new "$alt_sock" -- sh -c '
  ready=$1
  printf "\033[?1049h"
  printf "\033[2J\033[HSTALE-BEFORE-CLEAR"
  printf "\033[2J\033[H"
  printf "\033[1;1HCODEX-LIKE-HEADER"
  printf "\033[5;10HSTATUS READY"
  printf "\033[24;1Hprompt> "
  : > "$ready"
  sleep 10
' sh "$alt_ready" >"$alt_first"

for _ in {1..100}; do
  if [ -S "$alt_sock" ]; then
    break
  fi
  sleep 0.02
done

if [ ! -S "$alt_sock" ]; then
  echo "alternate-screen session socket was not created" >&2
  exit 1
fi

{ sleep 0.1; printf '\034'; } | "$bin" --rows 24 --cols 80 run "$alt_sock" >"$alt_second"

if ! grep -q 'CODEX-LIKE-HEADER' "$alt_second"; then
  echo "alternate-screen reattach did not retain header" >&2
  echo "--- alternate reattach output ---" >&2
  sed -n '1,160p' "$alt_second" >&2
  exit 1
fi

if ! grep -q 'STATUS READY' "$alt_second"; then
  echo "alternate-screen reattach did not retain positioned status text" >&2
  echo "--- alternate reattach output ---" >&2
  sed -n '1,160p' "$alt_second" >&2
  exit 1
fi

if grep -q 'STALE-BEFORE-CLEAR' "$alt_second"; then
  echo "alternate-screen reattach included stale cleared content" >&2
  echo "--- alternate reattach output ---" >&2
  sed -n '1,160p' "$alt_second" >&2
  exit 1
fi

if ! grep -q '\[detached\]' "$alt_second"; then
  echo "alternate-screen reattach did not detach cleanly" >&2
  echo "--- alternate reattach output ---" >&2
  sed -n '1,160p' "$alt_second" >&2
  exit 1
fi

if ! grep -Fq "[detached] $alt_sock" "$alt_second"; then
  echo "alternate-screen reattach did not print socket path" >&2
  echo "--- alternate reattach output ---" >&2
  sed -n '1,160p' "$alt_second" >&2
  exit 1
fi

printf '\003' | "$bin" run "$alt_sock" >/dev/null || true
