#!/usr/bin/env bash
# Lance poolsync-agent (une seule instance) avec l'environnement XFCE de CET utilisateur.
# Ne s'attache jamais au display d'un autre compte (ex. other-user).
# Sur xrdp : suit la session RDP actuellement connectée (xrdp-chansrv vivant),
# pas un display figé dans agent.toml.
set -euo pipefail
# Large clipboard buffers must return to the OS after image/text transitions.
# Bound glibc arena retention while preserving explicit administrator choices.
export MALLOC_ARENA_MAX="${MALLOC_ARENA_MAX:-2}"
export MALLOC_MMAP_THRESHOLD_="${MALLOC_MMAP_THRESHOLD_:-131072}"
UID_NUM="$(id -u)"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$UID_NUM}"

# Une session Wayland lancée par l'autostart transmet déjà son socket. Ne pas
# la rabattre artificiellement vers XWayland : le backend wl-clipboard en a besoin.
if [[ -n "${WAYLAND_DISPLAY:-}" && -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]]; then
  export XDG_SESSION_TYPE=wayland
  export GDK_BACKEND=wayland
  export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"
  echo "poolsync-agent-launch: uid=$UID_NUM WAYLAND_DISPLAY=$WAYLAND_DISPLAY" >&2
  exec "$HOME/.local/bin/poolsync-agent" "$@"
fi

env_of() {
  local pid="$1" key="$2"
  tr "\0" "\n" < "/proc/$pid/environ" 2>/dev/null \
    | grep "^${key}=" | head -1 | cut -d= -f2- || true
}

PICK_BIN="${HOME}/.local/bin/poolsync-pick-session.sh"
if [[ ! -x "$PICK_BIN" ]]; then
  PICK_BIN="$(cd "$(dirname "$0")" && pwd)/poolsync-pick-session.sh"
fi
if [[ ! -x "$PICK_BIN" ]]; then
  echo "poolsync-agent-launch: poolsync-pick-session.sh introuvable" >&2
  exit 1
fi

out="$("$PICK_BIN")" || {
  echo "poolsync-agent-launch: aucune session XFCE pour uid $UID_NUM" >&2
  exit 1
}
SESSION_DISPLAY="${out%% *}"
SESSION_PID="${out##* }"

if [[ -z "$SESSION_PID" || ! -d "/proc/$SESSION_PID" ]]; then
  echo "poolsync-agent-launch: session XFCE invalide ($out)" >&2
  exit 1
fi

export DISPLAY="$SESSION_DISPLAY"
dbus="$(env_of "$SESSION_PID" DBUS_SESSION_BUS_ADDRESS)"
if [[ -n "$dbus" ]]; then
  export DBUS_SESSION_BUS_ADDRESS="$dbus"
else
  export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
fi
xa="$(env_of "$SESSION_PID" XAUTHORITY)"
if [[ -n "$xa" && -f "$xa" ]]; then
  export XAUTHORITY="$xa"
else
  export XAUTHORITY="${HOME}/.Xauthority"
fi
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-XFCE}"
export GDK_BACKEND=x11

echo "poolsync-agent-launch: uid=$UID_NUM DISPLAY=$DISPLAY xfce4-session=$SESSION_PID" >&2

# Notifications : xfce4-notifyd souvent absent après reboot → notify-send timeout.
if ! pgrep -u "$UID_NUM" -x xfce4-notifyd >/dev/null 2>&1; then
  for notifyd in \
    /usr/lib/x86_64-linux-gnu/xfce4/notifyd/xfce4-notifyd \
    /usr/lib/xfce4/notifyd/xfce4-notifyd
  do
    if [[ -x "$notifyd" ]]; then
      "$notifyd" >/dev/null 2>&1 &
      break
    fi
  done
fi

# systemd restart must actually exec a new binary. Also: an agent left on
# a dead DISPLAY (RDP reconnect) looks "running" and would skip start.
if pid="$(pgrep -u "$UID_NUM" -x poolsync-agent | head -1)"; then
  cur="$(env_of "$pid" DISPLAY)"
  cur="${cur%.0}"
  want="${DISPLAY%.0}"
  if [[ -n "$cur" && "$cur" == "$want" && -r "/proc/$pid/exe" ]]; then
    echo "poolsync-agent-launch: already on $want pid=$pid" >&2
    exit 0
  fi
  echo "poolsync-agent-launch: replace pid=$pid DISPLAY=${cur:-?} → $want" >&2
  kill "$pid" 2>/dev/null || true
  sleep 0.4
  kill -9 "$pid" 2>/dev/null || true
  # Native copying applications also own xclip workers. Leave their selections
  # intact when replacing an agent attached to an obsolete graphical session.
fi
exec "$HOME/.local/bin/poolsync-agent" "$@"
