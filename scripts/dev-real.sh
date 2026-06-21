#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "REAL_COOLD=1 dev mode must run inside the Lima Linux VM. Use: bun run dev:vm" >&2
  exit 1
fi

command -v cargo >/dev/null 2>&1 || { echo "cargo is required." >&2; exit 1; }
command -v corrosion >/dev/null 2>&1 || { echo "corrosion is required in the VM." >&2; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "openssl is required." >&2; exit 1; }
command -v perl >/dev/null 2>&1 || { echo "perl is required for process-group management." >&2; exit 1; }
cargo watch --version >/dev/null 2>&1 || { echo "cargo-watch is required. Install it with: cargo install cargo-watch" >&2; exit 1; }

DEV_DIR="$ROOT/.dev"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/coold-target}"
export CARGO_TARGET_DIR
CORROSION_DIR="${COOLIFY_COOLD_VM_CORROSION_DIR:-/var/lib/corrosion}"
CORROSION_RUN_DIR="${COOLIFY_COOLD_VM_CORROSION_RUN_DIR:-/run/corrosion}"
CORROSION_CONFIG="$DEV_DIR/corrosion.vm.toml"
COOLIFY_FLUX_GRPC_BIND="${COOLIFY_FLUX_GRPC_BIND:-127.0.0.1:6443}"
COOLIFY_FLUX_UNIX_SOCKET_PATH="${COOLIFY_FLUX_UNIX_SOCKET_PATH:-/tmp/coolify-flux.sock}"
COOLIFY_COOLD_HOST_ID="${COOLIFY_COOLD_HOST_ID:-host-local}"
COOLIFY_COOLD_HOST_MGMT_IP="${COOLIFY_COOLD_HOST_MGMT_IP:-127.0.0.1}"
COOLIFY_COOLD_NETWORK_NAME="${COOLIFY_COOLD_NETWORK_NAME:-coolify-default-mesh}"
COOLIFY_COOLD_NETWORK_SUBNET="${COOLIFY_COOLD_NETWORK_SUBNET:-10.210.0.0/24}"
COOLIFY_COOLD_NETWORK_GATEWAY="${COOLIFY_COOLD_NETWORK_GATEWAY:-10.210.0.1}"
COOLIFY_COOLD_NAMESPACES="${COOLIFY_COOLD_NAMESPACES:-default:${COOLIFY_COOLD_NETWORK_NAME}:${COOLIFY_COOLD_NETWORK_GATEWAY}}"
COOLIFY_COOLD_PODMAN_SOCKET="${COOLIFY_COOLD_PODMAN_SOCKET:-/run/podman/podman.sock}"
COOLIFY_COOLD_FIREWALL_API_BIND="${COOLIFY_COOLD_FIREWALL_API_BIND:-127.0.0.1:8443}"
COOLIFY_COOLD_API_TOKEN_FILE="${COOLIFY_COOLD_API_TOKEN_FILE:-$DEV_DIR/coold-api-token}"
COOLIFY_COOLD_RULES_PATH="${COOLIFY_COOLD_RULES_PATH:-/etc/coolify/allow.rules}"
COOLIFY_COOLD_BRIDGE_RULES_PATH="${COOLIFY_COOLD_BRIDGE_RULES_PATH:-/etc/coolify/allow.nft}"
JWT_PRIV="$DEV_DIR/dev-jwt.priv.pem"
JWT_PUB="$DEV_DIR/dev-jwt.pub.pem"
JWT_FILE="$DEV_DIR/host-jwt"

pids=()
labels=()
cleaning_up=0

log_ts() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }
log_line() {
  local service="$1" level="$2"
  shift 2
  printf '%s %-5s [%s] %s\n' "$(log_ts)" "$level" "$service" "$*"
}
infer_level() {
  local line="$1"
  case "$line" in
    *" ERROR "*|ERROR:*|Error:*|error:*|*" error:"*) echo ERROR ;;
    *" WARN "*|WARN:*|Warning:*|warning:*) echo WARN ;;
    *" DEBUG "*|DEBUG:*) echo DEBUG ;;
    *" TRACE "*|TRACE:*) echo TRACE ;;
    *) echo INFO ;;
  esac
}
prefix_stream() {
  local service="$1" line level
  while IFS= read -r line; do
    level="$(infer_level "$line")"
    log_line "$service" "$level" "$line"
  done
}
host_part() { echo "${1%:*}"; }
port_part() { echo "${1##*:}"; }
is_tcp_open() {
  local host="$1" port="$2"
  (echo >"/dev/tcp/$host/$port") >/dev/null 2>&1
}
preflight_port_free() {
  local bind="$1" label="$2" override_hint="$3"
  local host port
  host="$(host_part "$bind")"
  port="$(port_part "$bind")"
  if is_tcp_open "$host" "$port"; then
    log_line dev ERROR "$label port is already in use at $host:$port." >&2
    log_line dev ERROR "Stop the existing process or override it, e.g.: $override_hint REAL_COOLD=1 bun run dev" >&2
    exit 1
  fi
}
is_pid_alive() { kill -0 "$1" 2>/dev/null; }
terminate_group() {
  local signal="$1" pid="$2"
  kill "-$signal" -- "-$pid" 2>/dev/null || true
  pkill "-$signal" -P "$pid" 2>/dev/null || true
  kill "-$signal" "$pid" 2>/dev/null || true
}
cleanup() {
  local status=$?
  if [[ "$cleaning_up" -eq 1 ]]; then exit "$status"; fi
  cleaning_up=1
  trap - EXIT INT TERM
  if ((${#pids[@]} > 0)); then
    for pid in "${pids[@]}"; do terminate_group TERM "$pid"; done
    local deadline=$((SECONDS + 5))
    while ((SECONDS < deadline)); do
      local alive=0
      for pid in "${pids[@]}"; do
        if is_pid_alive "$pid"; then alive=1; break; fi
      done
      [[ "$alive" -eq 0 ]] && break
      sleep 0.2
    done
    for pid in "${pids[@]}"; do
      if is_pid_alive "$pid"; then terminate_group KILL "$pid"; fi
    done
    wait "${pids[@]}" 2>/dev/null || true
  fi
  rm -f "$COOLIFY_FLUX_UNIX_SOCKET_PATH"
  exit "$status"
}
trap cleanup EXIT INT TERM
check_children() {
  local i pid label status
  for i in "${!pids[@]}"; do
    pid="${pids[$i]}"
    label="${labels[$i]}"
    if ! is_pid_alive "$pid"; then
      status=0
      wait "$pid" 2>/dev/null || status=$?
      log_line dev ERROR "$label exited during startup (status $status)." >&2
      return "$status"
    fi
  done
  return 0
}
wait_for_tcp() {
  local host="$1" port="$2" label="$3"
  for _ in {1..120}; do
    check_children
    if is_tcp_open "$host" "$port"; then return 0; fi
    sleep 0.25
  done
  log_line dev ERROR "timed out waiting for $label at $host:$port" >&2
  return 1
}
wait_for_socket() {
  local path="$1" label="$2"
  for _ in {1..120}; do
    check_children
    [[ -S "$path" ]] && return 0
    sleep 0.25
  done
  log_line dev ERROR "timed out waiting for $label at $path" >&2
  return 1
}
start_service() {
  local label="$1" script="$2"
  perl -MPOSIX=setsid -e 'setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"' bash -lc "$script" > >(prefix_stream "$label") 2> >(prefix_stream "$label" >&2) &
  local pid=$!
  pids+=("$pid")
  labels+=("$label")
  log_line dev INFO "Started $label (pid $pid)"
}

mkdir -p "$DEV_DIR" "$CARGO_TARGET_DIR"
sudo mkdir -p "$CORROSION_DIR" "$CORROSION_RUN_DIR"
sudo chmod 777 "$CORROSION_DIR" "$CORROSION_RUN_DIR"
rm -f "$COOLIFY_FLUX_UNIX_SOCKET_PATH"

preflight_port_free "$COOLIFY_FLUX_GRPC_BIND" "flux gRPC" "COOLIFY_FLUX_GRPC_BIND=127.0.0.1:6444"
preflight_port_free "127.0.0.1:8080" "Corrosion API" "COOLIFY_COOLD_CORROSION_URL=http://127.0.0.1:8081"
preflight_port_free "$COOLIFY_COOLD_FIREWALL_API_BIND" "coold firewall API" "COOLIFY_COOLD_FIREWALL_API_BIND=127.0.0.1:8444"

if [[ ! -S "$COOLIFY_COOLD_PODMAN_SOCKET" ]]; then
  log_line dev INFO "Starting rootful podman.socket via systemd…"
  sudo systemctl enable --now podman.socket >/dev/null
fi
if [[ ! -S "$COOLIFY_COOLD_PODMAN_SOCKET" ]]; then
  log_line dev ERROR "Podman socket not found at $COOLIFY_COOLD_PODMAN_SOCKET" >&2
  exit 1
fi

log_line dev INFO "Ensuring dev Podman network $COOLIFY_COOLD_NETWORK_NAME ($COOLIFY_COOLD_NETWORK_SUBNET gateway $COOLIFY_COOLD_NETWORK_GATEWAY)…"
if ! sudo podman network exists "$COOLIFY_COOLD_NETWORK_NAME"; then
  sudo podman network create --subnet "$COOLIFY_COOLD_NETWORK_SUBNET" --gateway "$COOLIFY_COOLD_NETWORK_GATEWAY" "$COOLIFY_COOLD_NETWORK_NAME" >/dev/null
fi

cat >"$CORROSION_CONFIG" <<CFG
[db]
path = "$CORROSION_DIR/state.db"
schema_paths = ["$ROOT/dev/corrosion/schema"]

[gossip]
addr = "127.0.0.1:8787"
external_addr = "127.0.0.1:8787"
bootstrap = []
plaintext = true

[api]
addr = "127.0.0.1:8080"

[admin]
path = "$CORROSION_RUN_DIR/admin.sock"
CFG

log_line dev INFO "Generating local dev JWT for real coold…"
JWT_TMP="$DEV_DIR/dev-jwt.$$"
JWT_TMP_PRIV="$JWT_TMP.priv.pem"
JWT_TMP_PUB="$JWT_TMP.pub.pem"
JWT="$(rtk cargo run -p flux --example sign_jwt -- "$COOLIFY_COOLD_HOST_ID" "$JWT_TMP_PRIV" "$JWT_TMP_PUB" "coold" 2>/dev/null | tail -n1)"
if [[ -z "$JWT" || ! -s "$JWT_TMP_PUB" ]]; then
  rm -f "$JWT_TMP_PRIV" "$JWT_TMP_PRIV.pkcs8" "$JWT_TMP_PUB"
  log_line dev ERROR "failed to generate JWT" >&2
  exit 1
fi
mv "$JWT_TMP_PRIV" "$JWT_PRIV"
mv "$JWT_TMP_PRIV.pkcs8" "$JWT_PRIV.pkcs8"
mv "$JWT_TMP_PUB" "$JWT_PUB"
printf '%s\n' "$JWT" >"$JWT_FILE"
printf '%s\n' "dev-firewall-token" >"$COOLIFY_COOLD_API_TOKEN_FILE"
chmod 600 "$JWT_FILE" "$COOLIFY_COOLD_API_TOKEN_FILE"

log_line dev INFO "Coolify v5 Lima real-coold dev stack"
log_line dev INFO "flux: grpc://$COOLIFY_FLUX_GRPC_BIND + unix://$COOLIFY_FLUX_UNIX_SOCKET_PATH"
log_line dev INFO "corrosion: http://127.0.0.1:8080"
log_line dev INFO "coold: host_id=$COOLIFY_COOLD_HOST_ID host_mgmt_ip=$COOLIFY_COOLD_HOST_MGMT_IP namespaces=$COOLIFY_COOLD_NAMESPACES firewall_api=$COOLIFY_COOLD_FIREWALL_API_BIND"

export ROOT JWT JWT_PUB JWT_FILE COOLIFY_FLUX_GRPC_BIND COOLIFY_FLUX_UNIX_SOCKET_PATH CORROSION_CONFIG COOLIFY_COOLD_HOST_MGMT_IP COOLIFY_COOLD_NAMESPACES COOLIFY_COOLD_PODMAN_SOCKET COOLIFY_COOLD_FIREWALL_API_BIND COOLIFY_COOLD_API_TOKEN_FILE COOLIFY_COOLD_RULES_PATH COOLIFY_COOLD_BRIDGE_RULES_PATH

start_service "corrosion" 'exec corrosion -c "$CORROSION_CONFIG" agent'
wait_for_tcp 127.0.0.1 8080 "Corrosion API"

start_service "flux" 'exec env COOLIFY_FLUX_GRPC_BIND="$COOLIFY_FLUX_GRPC_BIND" COOLIFY_FLUX_UNIX_SOCKET_PATH="$COOLIFY_FLUX_UNIX_SOCKET_PATH" COOLIFY_FLUX_JWT_PUBLIC_KEY_PATH="$JWT_PUB" COOLIFY_FLUX_ALLOW_PUBLIC_BIND=1 rtk cargo watch -w flux -w proto -w Cargo.toml -w Cargo.lock -i target -x "run -p flux"'
wait_for_tcp "$(host_part "$COOLIFY_FLUX_GRPC_BIND")" "$(port_part "$COOLIFY_FLUX_GRPC_BIND")" "flux gRPC"
wait_for_socket "$COOLIFY_FLUX_UNIX_SOCKET_PATH" "flux UDS"

start_service "coold" 'exec env COOLIFY_COOLD_HOST_MGMT_IP="$COOLIFY_COOLD_HOST_MGMT_IP" COOLIFY_COOLD_PODMAN_SOCKET="$COOLIFY_COOLD_PODMAN_SOCKET" COOLIFY_COOLD_CORROSION_URL="http://127.0.0.1:8080" COOLIFY_COOLD_NAMESPACES="$COOLIFY_COOLD_NAMESPACES" COOLIFY_COOLD_FLUX_URL="http://$COOLIFY_FLUX_GRPC_BIND" COOLIFY_COOLD_HOST_JWT_PATH="$JWT_FILE" COOLIFY_COOLD_API_BIND="$COOLIFY_COOLD_FIREWALL_API_BIND" COOLIFY_COOLD_API_TOKEN_FILE="$COOLIFY_COOLD_API_TOKEN_FILE" COOLIFY_COOLD_RULES_PATH="$COOLIFY_COOLD_RULES_PATH" COOLIFY_COOLD_BRIDGE_RULES_PATH="$COOLIFY_COOLD_BRIDGE_RULES_PATH" rtk cargo watch -w coold -w proto -w Cargo.toml -w Cargo.lock -i target -s "cargo build -p coold && sudo -E $CARGO_TARGET_DIR/debug/coold"'

check_children

log_line dev INFO "flux UDS ready at $COOLIFY_FLUX_UNIX_SOCKET_PATH — point your Laravel control plane here."
log_line dev INFO "Optional sample: sudo podman run -d --name coold-dev-web --network $COOLIFY_COOLD_NETWORK_NAME docker.io/library/nginx:alpine"

while true; do
  check_children
  sleep 1
done
