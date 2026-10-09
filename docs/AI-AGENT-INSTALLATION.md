# Build, installation and configuration runbook

## Work as a human–AI tandem

The human selects participants, KVM permissions, layout and RDP policy. The agent
inventories sessions and dependencies, prepares private configurations, builds
an exact candidate and installs reversibly within the authorized scope. Read
[the scenarios](CONCEPT-AND-SCENARIOS.md) first. No AI runtime service or extra
user password is introduced; existing credentials remain necessary.

## Inventory and build

Record the target OS/architecture/glibc, user/home, intended graphical session,
existing executable/configuration fingerprints, identity, authorized routes,
TLS trust, layout and clipboard managers. Never print passwords, keys or full
process environments. A copied home can carry a different node's identity;
node name, token, certificate SAN and peer authorization must agree.

On a Debian-family builder, representative dependencies are:

```sh
sudo apt-get install build-essential pkg-config clang cmake libssl-dev \
  libgtk-3-dev libayatana-appindicator3-dev libxdo-dev libnotify-dev \
  xclip xdotool xinput x11-utils python3-gi gir1.2-gtk-3.0 openssl
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo build --locked --release -p poolsync-agent
sha256sum target/release/poolsync-agent
target/release/poolsync-agent --version
```

Use a builder compatible with the targets; the version string alone does not
identify a qualified executable. Avoid modifying desktop libraries just to
compile a candidate. Keep native receiver, TLS, reconnect, slow-copy and
controller-loss tests separate from physical acceptance.

## Private configuration and identity

Start from [the configuration guide](CONFIGURATION.md) and
`examples/agent.desk-a.toml`. Resolve every placeholder privately. For an existing
pool, preserve keys and identity; do not regenerate its CA. For an entirely new
pool only, the generator refuses an existing output directory:

```sh
POOLSYNC_CONFIG_DIR="$HOME/poolsync-new-config" \
  ./deploy/generate-security.sh "$HOME/poolsync-new-security" \
  unused-hub.invalid desk-a desk-b work-a
```

Prepare reciprocal `agent.<node>.toml` routes in that private config directory
before generation so certificates include the correct DNS/IP SANs. The helper
still emits unused legacy hub certificate files; this does not start a hub.
Keep the CA private key offline. Distribute only each node's own key/certificate,
the trust certificate, its token and the intended shared encryption key. Replace
fragment TLS paths with that node's real private paths and merge each TOML table
once. Trust the CA through the target's reviewed system trust procedure; URLs
must match certificate SANs. Do not disable certificate validation.

## Fresh user installation

Use these commands as the target user in its intended graphical session. They
refuse an existing configuration. Prepare a private bundle containing a
qualified `poolsync-agent`, resolved `agent.toml`, consistent
`agent.topology.json` and a `tls/` directory with this node's material:

```sh
(
set -eu
: "${POOLSYNC_NODE_BUNDLE:?absolute private bundle path required}"
test ! -e "$HOME/.config/poolsync/agent.toml"
install -d -m 700 "$HOME/.config/poolsync" "$HOME/.config/poolsync/tls"
install -d "$HOME/.local/bin" "$HOME/.config/systemd/user"
install -m 755 "$POOLSYNC_NODE_BUNDLE/poolsync-agent" "$HOME/.local/bin/poolsync-agent"
install -m 600 "$POOLSYNC_NODE_BUNDLE/agent.toml" "$HOME/.config/poolsync/agent.toml"
install -m 600 "$POOLSYNC_NODE_BUNDLE/agent.topology.json" "$HOME/.config/poolsync/agent.topology.json"
cp -a "$POOLSYNC_NODE_BUNDLE/tls/." "$HOME/.config/poolsync/tls/"
chmod 600 "$HOME/.config/poolsync/tls/"*
install -m 755 deploy/poolsync-agent-launch.sh deploy/poolsync-pick-session.sh \
  deploy/poolsync-session-start.sh "$HOME/.local/bin/"
install -m 644 deploy/systemd/poolsync-agent.service "$HOME/.config/systemd/user/"
systemctl --user daemon-reload
systemctl --user enable --now poolsync-agent.service
)
```

The session picker prefers this user's live XRDP session, otherwise XFCE; it is
not a bridge across every session on a computer. If the desktop has no persistent
session, review the supplied graphical-session drop-in and session autostart
integration. Do not hard-code a UID or display. No deployment-specific VPN watchdog is bundled. PoolSync reconnects peers
when their network paths become available again.

## Existing pool upgrades

Do not apply fresh-install commands to existing identities. Record current
configuration, layout, service/session states and exact executable fingerprints.
Stage a qualified binary and a private rollback on every intended member. Make
the cohort temporarily absent, release held input and replace agents while all
members remain absent. Resume only after verifying a uniform candidate cohort.
The single-node `deploy/apply-agent-hotfix.py` requires an explicit `--user`,
candidate/current hashes and unique backup ID; it is not a fleet coordinator.
An interrupted or mixed cohort must remain absent until inspected and restored.

## Verification

Check the actual process executable and hash, intended graphical display,
unchanged identity/configuration/layout, peer presence, TLS and absence state.
Paste distinct images and text in GTK and a browser under the selected RDP policy.
Verify no older operation overwrites a newer copy, private rejoin exclusion and
controller loss/reconnection. Test both physical KVM directions, destination
typing, emergency return and monitor hotplug with the human present. A short
successful transfer or generated event is not daily-use qualification.
