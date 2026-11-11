#!/usr/bin/env bash
#
# tools/tv-mock.sh -- run the app on the TV against tests/mock_pms.py with ONE command.
#
#   tools/tv-mock.sh [--port N] [--screen S] [--dry-run] [-- MOCK_PMS_ARGS...]
#
# It replaces the three commands that used to be started by hand, in three terminals, with ports
# that had to agree:
#
#   python tests/mock_pms.py --port 32499                                  # [mock]
#   tools/tv-ssh ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 \
#       -R 127.0.0.1:32499:127.0.0.1:32499 tv                              # [tunnel]
#   tools/tv-session.sh up --guest --mock                                  # [tv]
#
# The port is chosen ONCE and handed to all three: --port, else $PLX_MOCK_PORT, else PMS_PORT from
# src/config.local.h, else 32499. config.local.h is where the port really lives: PMS_HOST/PMS_PORT are
# COMPILED INTO the app (the debug IPK from .github/workflows/demo-build.yml has 127.0.0.1:32400), and
# tools/mock-guest.py checks the mock at that same origin. So by default the script follows that file.
# It never edits it (a change there rebuilds and redeploys the binary on the next `up`); given a port
# the file disagrees with, it refuses to start and prints the two lines to put there.
#
# The loopback layout: the mock binds 127.0.0.1 on this machine, and the reverse tunnel makes
# 127.0.0.1:<port> ON THE TV reach it, so the app's compiled-in 127.0.0.1:<port> is the mock without
# opening a LAN port on this machine. (The LAN layout in the setup-environment skill, PMS_HOST = this
# machine's LAN address and `mock_pms.py --host <lan ip>`, needs no tunnel and no script.)
#
# Order: mock up and answering as the synthetic server -> tunnel up and the TV reaching the mock
# through it -> `tv-session.sh up --guest --mock` (which deploys if stale, takes the TV lock, arms the
# synthetic guest and launches). Each line is labelled [mock], [tunnel] or [tv].
#
# Then it SUPERVISES rather than quitting at the first problem: a TV that does not answer is waited
# for, a dropped tunnel is reopened once the TV answers again, a mock that exits is restarted (up to 3
# times in a row), a failed `up` is retried twice 10 s apart and then left for you, and every 15 s it
# asks the TV whether the app is running and says when it closes or comes back (relaunch it from the
# TV; the mock and tunnel stay up). q or Ctrl+C quits; that, or a mock that will not restart, stops the
# mock and the tunnel, so neither is left orphaned. The app is left running on the TV; hand it back with
# `tools/tv-session.sh down` when done.
#
# `up` deploys pkg/plxnative when the TV's copy differs and refuses when there is no local build at
# all, so the script checks for one first rather than starting anything for an `up` that cannot work.
#
#   --port N        the one port (default: $PLX_MOCK_PORT, else config.local.h's PMS_PORT, else 32499)
#   --screen S      passed to `tv-session.sh up` (default: up's own default, home)
#   --record FILE   on quit, save every request the app made (GET /_mock/requests) to FILE, for
#                   `tools/demo_bundle.py --requests FILE` (docs/demo-pms.md)
#   --demo          serve the demo library (real titles + artwork, tests/demo_library/catalog.json) instead
#                   of the seeded synthetic one; needs `python3 tools/demo_library.py derive` run once
#   --dry-run       check the config and print the three commands, start nothing
#   -- ARGS...      everything after `--` goes to tests/mock_pms.py (e.g. -- --movies 321 --home-hubs 9)
#
# Runs from Git Bash on Windows as well as macOS/Linux. $PYTHON picks the interpreter; otherwise
# python3, falling back to python (the Windows Store `python3` stub that only opens the Store is
# skipped because it fails `-c pass`).

# `sh tools/tv-mock.sh` runs bash in POSIX mode on Git Bash; re-run under real bash.
if [ -z "${BASH_VERSION:-}" ] || case ":${SHELLOPTS:-}:" in *:posix:*) true ;; *) false ;; esac; then
  exec bash "$0" "$@"
fi

set -u
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CONFIG="$REPO/src/config.local.h"
SYNTHETIC="1.41.0.0000-synthetic"   # the version string mock_pms.py's /identity answers (mock-guest.py checks it too)

port="${PLX_MOCK_PORT:-}"
screen=""
dry_run=0
demo=0
record=""
mock_args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --port) [ $# -ge 2 ] || { echo "tv-mock: --port needs a number" >&2; exit 2; }; port="$2"; shift 2 ;;
    --port=*) port="${1#*=}"; shift ;;
    --screen) [ $# -ge 2 ] || { echo "tv-mock: --screen needs a name" >&2; exit 2; }; screen="$2"; shift 2 ;;
    --screen=*) screen="${1#*=}"; shift ;;
    --dry-run) dry_run=1; shift ;;
    --demo) demo=1; shift ;;
    --record) [ $# -ge 2 ] || { echo "tv-mock: --record needs a file" >&2; exit 2; }; record="$2"; shift 2 ;;
    --record=*) record="${1#*=}"; shift ;;
    -h|--help) sed -n '3,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --) shift; mock_args=("$@"); break ;;
    *) echo "tv-mock: unknown option: $1 (mock_pms.py options go after --)" >&2; exit 2 ;;
  esac
done
cfg_host="" cfg_port=""
if [ -f "$CONFIG" ]; then
  cfg_host=$(sed -n 's/^[[:space:]]*#define[[:space:]]\{1,\}PMS_HOST[[:space:]]\{1,\}"\([^"]*\)".*/\1/p' "$CONFIG" | tail -1)
  cfg_port=$(sed -n 's/^[[:space:]]*#define[[:space:]]\{1,\}PMS_PORT[[:space:]]\{1,\}\([0-9]\{1,\}\).*/\1/p' "$CONFIG" | tail -1)
fi
[ -n "$port" ] || port="${cfg_port:-32499}"
case "$port" in ''|*[!0-9]*) echo "tv-mock: port must be a number, got '$port'" >&2; exit 2 ;; esac
if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then echo "tv-mock: port $port is out of range" >&2; exit 2; fi
for a in "${mock_args[@]+"${mock_args[@]}"}"; do
  case "$a" in
    --port|--port=*|--host|--host=*)
      echo "tv-mock: do not pass $a to the mock; the port is --port here and the mock must bind 127.0.0.1" >&2; exit 2 ;;
  esac
done

say() { printf '\033[1m[tv-mock]\033[0m %s\n' "$*"; }
die() { printf '\033[1m[tv-mock]\033[0m \033[31m%s\033[0m\n' "$1" >&2; exit "${2:-1}"; }
warn() { printf '\033[1m[tv-mock]\033[0m \033[33m%s\033[0m\n' "$*"; }

# ------------------------------------------------- the compiled-in endpoint must agree ---
[ -f "$CONFIG" ] || die "src/config.local.h is missing; create it with:
    #define PMS_HOST \"127.0.0.1\"
    #define PMS_PORT $port" 2
if [ "$cfg_host" != 127.0.0.1 ] || [ "$cfg_port" != "$port" ]; then
  die "src/config.local.h says PMS_HOST \"${cfg_host:-?}\" / PMS_PORT ${cfg_port:-?}, but this run needs 127.0.0.1:$port.
  The app has that endpoint compiled in, so the ports would disagree. Set these two lines in src/config.local.h
  (the next \`up\` rebuilds and redeploys the binary), or drop --port / PLX_MOCK_PORT to use the configured one:
    #define PMS_HOST \"127.0.0.1\"
    #define PMS_PORT $port" 2
fi
[ -f "$REPO/pkg/plxnative" ] || die "no local build at pkg/plxnative: tools/tv-session.sh up deploys it and refuses without it.
  Build it (make), or put the plxnative binary from the debug IPK installed on the TV at pkg/plxnative." 2

# ------------------------------------------------------------------ interpreter ---
if [ -n "${PYTHON:-}" ]; then py="$PYTHON"
elif python3 -c pass >/dev/null 2>&1; then py=python3
elif python -c pass >/dev/null 2>&1; then py=python
else die "no working python3 or python on PATH (set PYTHON=...)" 2
fi

up_args=(up --guest --mock)
[ -n "$screen" ] && up_args+=(--screen "$screen")
[ "$demo" = 1 ] && mock_args=(--catalog "$REPO/tests/demo_library/catalog.json" "${mock_args[@]+"${mock_args[@]}"}")
# Windows Python reads files as cp1252 unless told otherwise, and the demo catalog is UTF-8.
export PYTHONUTF8=1
MOCK_CMD=("$py" -u "$REPO/tests/mock_pms.py" --port "$port" "${mock_args[@]+"${mock_args[@]}"}")
TUNNEL_CMD=("$REPO/tools/tv-ssh" ssh -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15
            -R "127.0.0.1:$port:127.0.0.1:$port" tv)
UP_CMD=(bash "$REPO/tools/tv-session.sh" "${up_args[@]}")

if [ "$dry_run" = 1 ]; then
  say "config agrees: src/config.local.h compiles in 127.0.0.1:$port"
  say "would run, in order:"
  printf '  [mock]   %s\n' "${MOCK_CMD[*]}"
  printf '  [tunnel] %s\n' "${TUNNEL_CMD[*]}"
  printf '  [tv]     %s\n' "${UP_CMD[*]}"
  exit 0
fi

# ---------------------------------------------------------- labelled children ---
command -v curl >/dev/null 2>&1 || die "curl is needed to check the mock is answering" 2

WORK=$(mktemp -d 2>/dev/null || mktemp -d -t tv-mock) || die "could not create a temp directory"
MOCK_PID="" TUNNEL_PID=""

# Prefix every line from a child with its label. sed -u so lines show up as they are written.
label() { sed -u "s/^/$(printf '\033[%sm[%s]\033[0m ' "$2" "$1")/"; }

# Start "$@" in the background, its stdout+stderr labelled, and leave the CHILD's own pid (not the
# labeller's) in $WORK/<name>.pid, so it can be watched with kill -0 and stopped with kill. (Not
# returned through $(...): the labeller would hold that pipe open and the substitution would hang.) Background jobs of a
# non-interactive shell ignore SIGINT, so Ctrl+C reaches only this script, whose trap stops them.
start_labelled() {
  local name="$1" colour="$2"; shift 2
  ( "$@" </dev/null 2>&1 & echo $! > "$WORK/$name.pid"; wait $! ) | label "$name" "$colour" &
  local i
  for i in $(seq 1 50); do [ -s "$WORK/$name.pid" ] && break; sleep 0.1; done
  [ -s "$WORK/$name.pid" ] || die "could not start the $name"
}

alive() { [ -n "$1" ] && kill -0 "$1" 2>/dev/null; }

stop_child() {   # $1 pid, $2 name
  alive "$1" || return 0
  kill "$1" 2>/dev/null
  local i
  for i in 1 2 3 4 5 6 7 8 9 10; do alive "$1" || return 0; sleep 0.2; done
  kill -9 "$1" 2>/dev/null
  say "$2 did not stop on TERM; killed"
}

CLEANED=0
cleanup() {
  [ "$CLEANED" = 1 ] && return
  CLEANED=1
  trap '' INT TERM
  if [ -n "$record" ] && alive "$MOCK_PID"; then
    if curl -s -m 10 "http://127.0.0.1:$port/_mock/requests" > "$record" 2>/dev/null; then
      say "saved the session's requests to $record (tools/demo_bundle.py --requests $record)"
    else
      warn "could not save the session's requests to $record"
    fi
  fi
  [ -n "$TUNNEL_PID" ] && { stop_child "$TUNNEL_PID" tunnel; say "tunnel stopped"; }
  [ -n "$MOCK_PID" ] && { stop_child "$MOCK_PID" mock; say "mock stopped"; }
  wait 2>/dev/null
  rm -rf "$WORK"
}
trap 'cleanup' EXIT
trap 'echo; say "interrupted — stopping"; exit 130' INT TERM
trap 'exit 129' HUP

# Something already answering on the port would make the new mock fail to bind, or (worse) leave the
# old one serving. Say which.
if curl -s -m 2 "http://127.0.0.1:$port/identity" >/dev/null 2>&1; then
  if curl -s -m 2 "http://127.0.0.1:$port/identity" | grep -q "$SYNTHETIC"; then
    die "a mock is already running on 127.0.0.1:$port (another tv-mock or a hand-started mock_pms.py); stop it first"
  fi
  die "something that is not the mock already answers on 127.0.0.1:$port; pick another --port"
fi


# Which install's binary to look for when asking whether the app is running (what tv-session.sh's
# app_pids does). Asked of the Makefile like everything else; CRLF stripped for a Windows make.
APPDIR=$(make -s -C "$REPO" print-appdir 2>/dev/null | tr -d '\r' | tail -1)

tv_reachable() { "$REPO/tools/tv-ssh" ssh tv true </dev/null >/dev/null 2>&1; }

# Start the mock and wait until it answers as the synthetic server. 0 = up.
start_mock() {
  rm -f "$WORK/mock.pid"
  start_labelled mock 36 "${MOCK_CMD[@]}"; MOCK_PID=$(cat "$WORK/mock.pid")
  local i
  for i in $(seq 1 60); do
    alive "$MOCK_PID" || return 1
    curl -s -m 2 "http://127.0.0.1:$port/identity" 2>/dev/null | grep -q "$SYNTHETIC" && return 0
    sleep 0.5
  done
  stop_child "$MOCK_PID" mock; return 1
}

# Open the tunnel and wait until the TV reaches the mock through it. 0 = up; on failure nothing is
# left running.
start_tunnel() {
  rm -f "$WORK/tunnel.pid"
  start_labelled tunnel 35 "${TUNNEL_CMD[@]}"; TUNNEL_PID=$(cat "$WORK/tunnel.pid")
  local i
  for i in $(seq 1 15); do
    sleep 1
    alive "$TUNNEL_PID" || return 1
    "$REPO/tools/tv-ssh" ssh tv "curl -s -m 3 http://127.0.0.1:$port/identity" </dev/null 2>/dev/null \
      | grep -q "$SYNTHETIC" && return 0
  done
  stop_child "$TUNNEL_PID" tunnel; return 1
}

# The app's pids on the TV ("" = not running). Returns 1 when the TV could not be asked.
app_pids() {
  local out
  out=$("$REPO/tools/tv-ssh" ssh tv "fuser $APPDIR/plxnative 2>/dev/null; echo ok" </dev/null 2>/dev/null) || return 1
  case "$out" in *ok*) ;; *) return 1 ;; esac
  printf '%s' "${out%ok*}" | tr -cs '0-9' ' ' | sed 's/^ *//; s/ *$//'
}

run_up() {
  say "bringing the TV session up: tv-session.sh ${up_args[*]}"
  "${UP_CMD[@]}" </dev/null 2>&1 | label tv 33
  return "${PIPESTATUS[0]}"
}

# One key, or a one-second pause when there is no terminal to read from. Sets KEY.
KEY=""
read_key() {
  KEY=""
  if [ -t 0 ]; then read -rsn1 -t 1 KEY 2>/dev/null || KEY=""; else sleep 1; fi
}

# ------------------------------------------------------------------- 1. mock ---
say "starting the mock on 127.0.0.1:$port"
start_mock || die "the mock did not start (its output is above)"
say "mock answering"

# ------------------------------------------------- 2. tunnel, 3. up, then supervise ---
# From here nothing gives up on its own: a dropped tunnel is reopened once the TV answers again, a
# mock that dies is restarted, a failed `up` is retried, and the app closing on the TV is reported.
# Only q, Ctrl+C, or a mock that will not start again ends the session.
HELP="press q to quit"
tunnel_up=0 tunnel_waiting=0 tunnel_next=0
up_state=pending up_tries=0 up_next=0
app_state=unknown app_next=0
mock_fails=0
tick=0
while :; do
  tick=$((tick + 1))

  # -- the mock
  if ! alive "$MOCK_PID"; then
    mock_fails=$((mock_fails + 1))
    [ "$mock_fails" -gt 3 ] && die "the mock exited and would not start again (3 tries); stopping"
    warn "the mock exited; restarting it (try $mock_fails of 3)"
    if start_mock; then say "mock answering again"; mock_fails=0; fi
    continue
  fi

  # -- the tunnel
  if [ "$tunnel_up" = 1 ] && ! alive "$TUNNEL_PID"; then
    tunnel_up=0; tunnel_next=$tick
    warn "the tunnel dropped (TV asleep or off the network?); reopening it when the TV answers"
  fi
  if [ "$tunnel_up" = 0 ] && [ "$tick" -ge "$tunnel_next" ]; then
    if tv_reachable; then
      say "opening the reverse tunnel (TV 127.0.0.1:$port -> this machine 127.0.0.1:$port)"
      if start_tunnel; then
        tunnel_up=1; tunnel_waiting=0
        say "the TV reaches the mock"
        app_next=$tick
      else
        warn "the tunnel did not come up (its [tunnel] line above says why); retrying in 10 s"
        tunnel_next=$((tick + 10))
      fi
    else
      [ "$tunnel_waiting" = 0 ] && warn "the TV does not answer (asleep? wake it); waiting for it — q quits"
      tunnel_waiting=1; tunnel_next=$((tick + 10))
    fi
  fi

  # -- tv-session up: the first launch and its retries
  if [ "$tunnel_up" = 1 ] && [ "$up_state" = pending ] && [ "$tick" -ge "$up_next" ]; then
    up_tries=$((up_tries + 1))
    if run_up; then
      up_state=done; up_tries=0; app_next=$((tick + 5))
      say "running — the mock and tunnel stay up; $HELP"
    elif [ "$up_tries" -lt 3 ]; then
      warn "up failed (try $up_tries of 3); retrying in 10 s — q quits"
      up_next=$((tick + 10))
    else
      warn "up failed 3 times; the mock and tunnel stay up — launch the app from the TV"
      up_state=waiting; up_tries=0
    fi
  fi

  # -- is the app running? (every 15 s, only while the TV is reachable)
  if [ "$tunnel_up" = 1 ] && [ "$tick" -ge "$app_next" ]; then
    app_next=$((tick + 15))
    if pids=$(app_pids); then
      if [ -z "$pids" ] && [ "$app_state" != stopped ]; then
        case "$app_state" in running:*) warn "the app closed on the TV; relaunch it there (mock and tunnel stay up)" ;; esac
        app_state=stopped
      elif [ -n "$pids" ] && [ "$app_state" != "running:$pids" ]; then
        case "$app_state" in stopped|running:*) say "the app is running (pid $pids)" ;; esac
        app_state="running:$pids"
      fi
    fi
  fi

  read_key
  case "$KEY" in
    q|Q) echo; say "quitting"; exit 0 ;;
    "") ;;
    *) say "$HELP" ;;
  esac
done
