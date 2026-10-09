#!/usr/bin/env bash
# Restart PoolSync after the graphical XFCE session is ready (systray needs X11).
# Prefers the live xrdp session of this user when present.
set -euo pipefail
UID_NUM="$(id -u)"
export XDG_RUNTIME_DIR="/run/user/$UID_NUM"
mkdir -p "$XDG_RUNTIME_DIR"
exec 9>"$XDG_RUNTIME_DIR/poolsync-session-start.lock"
flock -n 9 || exit 0
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"

PICK_BIN="${HOME}/.local/bin/poolsync-pick-session.sh"

pick_display() {
  local out
  if [[ -x "$PICK_BIN" ]] && out="$("$PICK_BIN" --live-rdp 2>/dev/null)"; then
    printf '%s' "${out%% *}"
    return 0
  fi
  if [[ -n "${DISPLAY:-}" ]]; then
    printf '%s' "${DISPLAY%.0}"
    return 0
  fi
  if [[ -x "$PICK_BIN" ]] && out="$("$PICK_BIN" 2>/dev/null)"; then
    printf '%s' "${out%% *}"
    return 0
  fi
  return 1
}

wait_for_x() {
  local i disp
  for i in $(seq 1 45); do
    disp="$(pick_display || true)"
    if [[ -n "$disp" ]]; then
      export DISPLAY="$disp"
      export XAUTHORITY="${XAUTHORITY:-$HOME/.Xauthority}"
      if xdpyinfo -display "$DISPLAY" >/dev/null 2>&1; then
        return 0
      fi
    fi
    sleep 2
  done
  return 1
}

if ! wait_for_x; then
  echo "poolsync-session-start: XFCE/X11 not ready" >&2
  exit 1
fi

# XFCE re-runs autostart on panel/RDP reconnect. Always restarting looks like a crash
# (systray vanishes). Only restart if missing or on the wrong DISPLAY.
want="${DISPLAY%.0}"
if pid="$(pgrep -u "$UID_NUM" -x poolsync-agent | head -1)"; then
  cur="$(tr '\0' '\n' < "/proc/$pid/environ" 2>/dev/null | grep '^DISPLAY=' | head -1 | cut -d= -f2- || true)"
  cur="${cur%.0}"
  if [[ -n "$cur" && "$cur" == "$want" ]] \
    && systemctl --user is-active --quiet poolsync-agent.service 2>/dev/null; then
    echo "poolsync-session-start: already running on $want pid=$pid" >&2
    exit 0
  fi
fi

systemctl --user restart poolsync-agent.service
