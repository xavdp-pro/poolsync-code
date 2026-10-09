#!/usr/bin/env bash
# Génère une PKI locale, des identités par nœud et une clef E2E.
# Usage: ./deploy/generate-security.sh /chemin/hors-depot hub.example node-a node-b
# Les SAN IP/DNS des nœuds sont déduits des peer_url de deploy/config.
# POOLSYNC_CONFIG_DIR permet de fournir un autre répertoire de configurations.
set -euo pipefail
umask 077

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONFIG_DIR="${POOLSYNC_CONFIG_DIR:-$SCRIPT_DIR/config}"

OUT_DIR="${1:?répertoire de sortie requis}"
HUB_NAME="${2:?nom DNS/IP du hub requis}"
shift 2
(( $# > 0 )) || { echo "au moins un nœud est requis" >&2; exit 1; }
command -v openssl >/dev/null || { echo "openssl requis" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 requis" >&2; exit 1; }

# New pools only: never replace existing identities or encryption material.
[[ ! -e "$OUT_DIR" ]] || { echo "Output already exists; preserve its keys and configuration." >&2; exit 1; }
mkdir -p "$OUT_DIR/nodes"
CA_KEY="$OUT_DIR/ca.key"
CA_CERT="$OUT_DIR/ca.crt"

openssl genpkey -algorithm ED25519 -out "$CA_KEY"
openssl req -x509 -new -key "$CA_KEY" -out "$CA_CERT" -days 3650 \
  -subj "/CN=PoolSync local CA" \
  -addext "basicConstraints=critical,CA:TRUE" \
  -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl genpkey -algorithm ED25519 -out "$OUT_DIR/hub.key"
SAN="DNS:${HUB_NAME},DNS:localhost,IP:127.0.0.1"
if [[ "$HUB_NAME" =~ ^[0-9a-fA-F:.]+$ ]]; then
  SAN="IP:${HUB_NAME},DNS:localhost,IP:127.0.0.1"
fi
openssl req -new -key "$OUT_DIR/hub.key" -out "$OUT_DIR/hub.csr" \
  -subj "/CN=${HUB_NAME}" -addext "subjectAltName=${SAN}"
printf 'subjectAltName=%s\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' "$SAN" > "$OUT_DIR/hub.ext"
openssl x509 -req -in "$OUT_DIR/hub.csr" -CA "$CA_CERT" -CAkey "$CA_KEY" \
  -CAcreateserial -out "$OUT_DIR/hub.crt" -days 825 -extfile "$OUT_DIR/hub.ext"
rm -f "$OUT_DIR/hub.csr" "$OUT_DIR/hub.ext" "$OUT_DIR/ca.srl"

E2E_KEY="$(openssl rand -base64 32 | tr -d '\n')"
printf '%s\n' "$E2E_KEY" > "$OUT_DIR/e2e.key"

TOKENS_TSV="$OUT_DIR/.node-tokens.tsv"
: > "$TOKENS_TSV"
for node in "$@"; do
  [[ "$node" =~ ^[A-Za-z0-9._-]+$ ]] || {
    echo "nom de nœud invalide: $node" >&2
    exit 1
  }
  token="$(openssl rand -base64 32 | tr -d '\n')"
  printf '%s\t%s\n' "$node" "$token" >> "$TOKENS_TSV"
  NODE_SAN="DNS:${node}"
  while IFS= read -r configured_san; do
    [[ -n "$configured_san" ]] || continue
    case ",$NODE_SAN," in
      *",$configured_san,"*) ;;
      *) NODE_SAN+=",$configured_san" ;;
    esac
  done < <(python3 - "$CONFIG_DIR" "$node" <<'PY'
import glob
import ipaddress
import pathlib
import sys
import tomllib
import urllib.parse

config_dir = pathlib.Path(sys.argv[1])
target = sys.argv[2]
sans = set()
for path in glob.glob(str(config_dir / "agent.*.toml")):
    try:
        with open(path, "rb") as handle:
            config = tomllib.load(handle)
    except (OSError, tomllib.TOMLDecodeError):
        continue
    for peer in config.get("neighbors", []):
        if peer.get("node") != target:
            continue
        for key in ("peer_url", "peer_url_vpn"):
            host = urllib.parse.urlparse(peer.get(key, "")).hostname
            if not host:
                continue
            try:
                ipaddress.ip_address(host)
                sans.add(f"IP:{host}")
            except ValueError:
                sans.add(f"DNS:{host}")
for san in sorted(sans):
    print(san)
PY
  )
  openssl genpkey -algorithm ED25519 -out "$OUT_DIR/nodes/$node.key"
  openssl req -new -key "$OUT_DIR/nodes/$node.key" -out "$OUT_DIR/nodes/$node.csr" \
    -subj "/CN=${node}" -addext "subjectAltName=${NODE_SAN}"
  printf 'subjectAltName=%s\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\n' "$NODE_SAN" > "$OUT_DIR/nodes/$node.ext"
  openssl x509 -req -in "$OUT_DIR/nodes/$node.csr" -CA "$CA_CERT" -CAkey "$CA_KEY" \
    -CAcreateserial -out "$OUT_DIR/nodes/$node.crt" -days 825 \
    -extfile "$OUT_DIR/nodes/$node.ext"
  rm -f "$OUT_DIR/nodes/$node.csr" "$OUT_DIR/nodes/$node.ext" "$OUT_DIR/ca.srl"
done

TOKENS_TSV="$TOKENS_TSV" OUT_DIR="$OUT_DIR" E2E_KEY="$E2E_KEY" python3 - <<'PY'
import json, os, pathlib
nodes = {}
with open(os.environ["TOKENS_TSV"], encoding="utf-8") as handle:
    for line in handle:
        node, token = line.rstrip("\n").split("\t", 1)
        nodes[node] = token
out = pathlib.Path(os.environ["OUT_DIR"])
(out / "node-tokens.json").write_text(json.dumps({
    "nodes": {
        node: {"token": token, "previous_tokens": [], "revoked": False}
        for node, token in nodes.items()
    }
}, indent=2) + "\n", encoding="utf-8")
for node, token in nodes.items():
    lines = [
        f'node_token = {json.dumps(token)}',
        f'e2e_key = {json.dumps(os.environ["E2E_KEY"])}',
        f'peer_tls_cert = "POOLSYNC_TLS_DIR/{node}.crt"',
        f'peer_tls_key = "POOLSYNC_TLS_DIR/{node}.key"',
        "",
        "[peer_tokens]",
    ]
    lines.extend(f'{json.dumps(peer)} = {json.dumps(peer_token)}' for peer, peer_token in nodes.items() if peer != node)
    (out / "nodes" / f"{node}.toml").write_text("\n".join(lines) + "\n", encoding="utf-8")
PY
rm -f "$TOKENS_TSV"
chmod 600 "$OUT_DIR"/*.key "$OUT_DIR"/node-tokens.json "$OUT_DIR"/nodes/*.key "$OUT_DIR"/nodes/*.toml
chmod 644 "$CA_CERT" "$OUT_DIR/hub.crt" "$OUT_DIR"/nodes/*.crt

echo "Matériel PoolSync créé dans $OUT_DIR"
echo "Conserve ca.key hors ligne; distribue seulement ca.crt et le fragment TOML propre à chaque nœud."
