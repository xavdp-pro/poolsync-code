#!/usr/bin/env bash
# Pick this uid's XFCE session. Prefer a live xrdp-chansrv (RDP currently
# connected). Never another account (other-user, root, …).
# Prints: DISPLAY PID
#   poolsync-pick-session.sh            → live RDP if any, else newest XFCE
#   poolsync-pick-session.sh --live-rdp → live RDP only (exit 1 if none)
set -euo pipefail

LIVE_ONLY=0
if [[ "${1:-}" == "--live-rdp" ]]; then
  LIVE_ONLY=1
fi

UID_NUM="$(id -u)"

env_of() {
  local pid="$1" key="$2"
  [[ -r "/proc/$pid/environ" ]] || return 0
  tr "\0" "\n" < "/proc/$pid/environ" 2>/dev/null \
    | grep "^${key}=" | head -1 | cut -d= -f2- || true
}

norm_display() {
  local d="${1:-}"
  d="${d%.0}"
  printf '%s' "$d"
}

is_zombie() {
  local st
  st="$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ' || true)"
  [[ -z "$st" || "$st" == *Z* ]]
}

declare -A LIVE=()
while IFS= read -r pid; do
  [[ -z "$pid" ]] && continue
  is_zombie "$pid" && continue
  disp="$(norm_display "$(env_of "$pid" DISPLAY)")"
  [[ -n "$disp" ]] && LIVE["$disp"]=1
done < <(pgrep -u "$UID_NUM" -x xrdp-chansrv 2>/dev/null || true)

BEST_START=0
BEST_PID=""
BEST_DISP=""
LIVE_START=0
LIVE_PID=""
LIVE_DISP=""

while IFS= read -r pid; do
  [[ -z "$pid" ]] && continue
  disp="$(norm_display "$(env_of "$pid" DISPLAY)")"
  [[ -n "$disp" ]] || continue
  start="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || echo 0)"
  [[ "$start" =~ ^[0-9]+$ ]] || start=0
  if (( start >= BEST_START )); then
    BEST_START=$start
    BEST_PID=$pid
    BEST_DISP=$disp
  fi
  if [[ -n "${LIVE[$disp]:-}" ]] && (( start >= LIVE_START )); then
    LIVE_START=$start
    LIVE_PID=$pid
    LIVE_DISP=$disp
  fi
done < <(pgrep -u "$UID_NUM" -x xfce4-session 2>/dev/null || true)

if [[ -n "$LIVE_DISP" ]]; then
  echo "$LIVE_DISP $LIVE_PID"
  exit 0
fi
if [[ "$LIVE_ONLY" == "1" ]]; then
  echo "poolsync-pick-session: no live RDP session for uid $UID_NUM" >&2
  exit 1
fi
if [[ -n "$BEST_DISP" ]]; then
  echo "$BEST_DISP $BEST_PID"
  exit 0
fi
echo "poolsync-pick-session: no XFCE session for uid $UID_NUM" >&2
exit 1
