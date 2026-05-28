#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

command -v bun >/dev/null 2>&1 || { echo "bun is required for Coolify UI dev." >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "cargo is required for Rust dev." >&2; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "openssl is required for local scheduler JWT generation." >&2; exit 1; }
cargo watch --version >/dev/null 2>&1 || { echo "cargo-watch is required. Install it with: cargo install cargo-watch" >&2; exit 1; }

DEV_DIR="$ROOT/.dev"
SCHEDULER_GRPC_BIND="${SCHEDULER_GRPC_BIND:-127.0.0.1:6443}"
SCHEDULER_UNIX_SOCKET_PATH="${SCHEDULER_UNIX_SOCKET_PATH:-/tmp/coolify-scheduler.sock}"
COOLIFY_API_BIND="${COOLIFY_API_BIND:-127.0.0.1:3000}"
COOLIFY_UI_PORT="${COOLIFY_UI_PORT:-5173}"
COOLIFY_API_DB="${COOLIFY_API_DB:-/tmp/api-dev.db}"
FAKE_COOLD_HOST_ID="${FAKE_COOLD_HOST_ID:-host-local}"
FAKE_COOLD_CAPS="${FAKE_COOLD_CAPS:-coold,builder}"
JWT_PRIV="$DEV_DIR/dev-jwt.priv.pem"
JWT_PUB="$DEV_DIR/dev-jwt.pub.pem"

mkdir -p "$DEV_DIR"
rm -f "$SCHEDULER_UNIX_SOCKET_PATH"

echo "Generating local dev JWT for fake coold…"
JWT="$(rtk cargo run -p scheduler --example sign_jwt -- "$FAKE_COOLD_HOST_ID" "$JWT_PRIV" "$JWT_PUB" "$FAKE_COOLD_CAPS" 2>/dev/null | tail -n1)"
if [[ -z "$JWT" ]]; then
  echo "failed to generate JWT" >&2
  exit 1
fi

pids=()
cleanup() {
  local status=$?
  trap - EXIT INT TERM
  for pid in "${pids[@]:-}"; do
    pkill -TERM -P "$pid" 2>/dev/null || true
    kill "$pid" 2>/dev/null || true
  done
  sleep 0.2
  for pid in "${pids[@]:-}"; do
    pkill -KILL -P "$pid" 2>/dev/null || true
    kill -KILL "$pid" 2>/dev/null || true
  done
  wait "${pids[@]:-}" 2>/dev/null || true
  rm -f "$SCHEDULER_UNIX_SOCKET_PATH"
  exit "$status"
}
trap cleanup EXIT INT TERM

wait_for_tcp() {
  local host="$1" port="$2" label="$3"
  for _ in {1..120}; do
    if (echo >"/dev/tcp/$host/$port") >/dev/null 2>&1; then return 0; fi
    sleep 0.25
  done
  echo "timed out waiting for $label at $host:$port" >&2
  return 1
}

wait_for_socket() {
  local path="$1" label="$2"
  for _ in {1..120}; do
    [[ -S "$path" ]] && return 0
    sleep 0.25
  done
  echo "timed out waiting for $label at $path" >&2
  return 1
}

echo "Coolify v5 dev stack"
echo "  Coolify UI: http://127.0.0.1:$COOLIFY_UI_PORT"
echo "  Coolify API: http://$COOLIFY_API_BIND"
echo "  scheduler:  grpc://$SCHEDULER_GRPC_BIND + unix://$SCHEDULER_UNIX_SOCKET_PATH"
echo "  fake coold: host_id=$FAKE_COOLD_HOST_ID caps=$FAKE_COOLD_CAPS"
echo

(
  exec env \
    SCHEDULER_GRPC_BIND="$SCHEDULER_GRPC_BIND" \
    SCHEDULER_UNIX_SOCKET_PATH="$SCHEDULER_UNIX_SOCKET_PATH" \
    SCHEDULER_JWT_PUBLIC_KEY_PATH="$JWT_PUB" \
    SCHEDULER_ALLOW_PUBLIC_BIND=1 \
    rtk cargo watch \
      -w scheduler \
      -w proto \
      -w Cargo.toml \
      -w Cargo.lock \
      -i target \
      -x 'run -p scheduler'
) & pids+=("$!")

wait_for_tcp "${SCHEDULER_GRPC_BIND%:*}" "${SCHEDULER_GRPC_BIND##*:}" "scheduler gRPC"
wait_for_socket "$SCHEDULER_UNIX_SOCKET_PATH" "scheduler UDS"

(
  while true; do
    env SCHEDULER_URL="http://$SCHEDULER_GRPC_BIND" JWT="$JWT" \
      rtk cargo run -p scheduler --example fake_coold || true
    sleep 1
  done
) & pids+=("$!")

(
  exec env \
    SKIP_UI=1 \
    COOLIFY_API_BIND="$COOLIFY_API_BIND" \
    COOLIFY_API_DB="$COOLIFY_API_DB" \
    COOLIFY_SCHEDULER_SOCKET="$SCHEDULER_UNIX_SOCKET_PATH" \
    rtk cargo watch \
      -w core \
      -w storage \
      -w api \
      -w migrations \
      -w Cargo.toml \
      -w Cargo.lock \
      -i target \
      -x 'run -p api -- serve'
) & pids+=("$!")

(
  cd "$ROOT/coolify-ui"
  exec env COOLIFY_UI_PORT="$COOLIFY_UI_PORT" bun run dev
) & pids+=("$!")

wait_for_tcp "${COOLIFY_API_BIND%:*}" "${COOLIFY_API_BIND##*:}" "api"

echo
echo "Dev stack is up. Open http://127.0.0.1:$COOLIFY_UI_PORT"
echo "Tip: Servers → Sync scheduler streams → open host-local."
echo

while true; do
  for pid in "${pids[@]}"; do
    if ! kill -0 "$pid" 2>/dev/null; then
      wait "$pid"
      exit $?
    fi
  done
  sleep 1
done
