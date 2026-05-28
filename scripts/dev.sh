#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

command -v bun >/dev/null 2>&1 || { echo "bun is required for Coolify UI dev." >&2; exit 1; }
command -v cargo >/dev/null 2>&1 || { echo "cargo is required for Rust dev." >&2; exit 1; }
command -v openssl >/dev/null 2>&1 || { echo "openssl is required for local scheduler JWT generation." >&2; exit 1; }
command -v perl >/dev/null 2>&1 || { echo "perl is required for process-group management." >&2; exit 1; }
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

pids=()
labels=()
cleaning_up=0

log_ts() {
  date -u +"%Y-%m-%dT%H:%M:%SZ"
}

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

host_part() {
  echo "${1%:*}"
}

port_part() {
  echo "${1##*:}"
}

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
    log_line dev ERROR "Stop the existing process or override it, e.g.: $override_hint bash scripts/dev.sh" >&2
    exit 1
  fi
}

preflight_ui_port_free() {
  if is_tcp_open "127.0.0.1" "$COOLIFY_UI_PORT"; then
    log_line dev ERROR "Coolify UI port is already in use at 127.0.0.1:$COOLIFY_UI_PORT." >&2
    log_line dev ERROR "Stop the existing process or override it, e.g.: COOLIFY_UI_PORT=5174 bash scripts/dev.sh" >&2
    exit 1
  fi
}

is_pid_alive() {
  kill -0 "$1" 2>/dev/null
}

terminate_group() {
  local signal="$1" pid="$2"
  kill "-$signal" -- "-$pid" 2>/dev/null || true
  pkill "-$signal" -P "$pid" 2>/dev/null || true
  kill "-$signal" "$pid" 2>/dev/null || true
}

cleanup() {
  local status=$?
  if [[ "$cleaning_up" -eq 1 ]]; then
    exit "$status"
  fi
  cleaning_up=1
  trap - EXIT INT TERM

  if ((${#pids[@]} > 0)); then
    for pid in "${pids[@]}"; do
      terminate_group TERM "$pid"
    done

    local deadline=$((SECONDS + 5))
    while ((SECONDS < deadline)); do
      local alive=0
      for pid in "${pids[@]}"; do
        if is_pid_alive "$pid"; then
          alive=1
          break
        fi
      done
      [[ "$alive" -eq 0 ]] && break
      sleep 0.2
    done

    for pid in "${pids[@]}"; do
      if is_pid_alive "$pid"; then
        terminate_group KILL "$pid"
      fi
    done

    wait "${pids[@]}" 2>/dev/null || true
  fi

  rm -f "$SCHEDULER_UNIX_SOCKET_PATH"
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
  perl -MPOSIX=setsid -e 'setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"' bash -c "$script" > >(prefix_stream "$label") 2> >(prefix_stream "$label" >&2) &
  local pid=$!
  pids+=("$pid")
  labels+=("$label")
  log_line dev INFO "Started $label (pid $pid)"
}

mkdir -p "$DEV_DIR"
rm -f "$SCHEDULER_UNIX_SOCKET_PATH"

preflight_port_free "$SCHEDULER_GRPC_BIND" "scheduler gRPC" "SCHEDULER_GRPC_BIND=127.0.0.1:6444"
preflight_port_free "$COOLIFY_API_BIND" "Coolify API" "COOLIFY_API_BIND=127.0.0.1:3001"
preflight_ui_port_free

log_line dev INFO "Generating local dev JWT for fake coold…"
JWT_TMP="$DEV_DIR/dev-jwt.$$"
JWT_TMP_PRIV="$JWT_TMP.priv.pem"
JWT_TMP_PUB="$JWT_TMP.pub.pem"
JWT="$(rtk cargo run -p scheduler --example sign_jwt -- "$FAKE_COOLD_HOST_ID" "$JWT_TMP_PRIV" "$JWT_TMP_PUB" "$FAKE_COOLD_CAPS" 2>/dev/null | tail -n1)"
if [[ -z "$JWT" || ! -s "$JWT_TMP_PUB" ]]; then
  rm -f "$JWT_TMP_PRIV" "$JWT_TMP_PRIV.pkcs8" "$JWT_TMP_PUB"
  log_line dev ERROR "failed to generate JWT" >&2
  exit 1
fi
mv "$JWT_TMP_PRIV" "$JWT_PRIV"
mv "$JWT_TMP_PRIV.pkcs8" "$JWT_PRIV.pkcs8"
mv "$JWT_TMP_PUB" "$JWT_PUB"

log_line dev INFO "Coolify v5 dev stack"
log_line dev INFO "Coolify UI: http://127.0.0.1:$COOLIFY_UI_PORT"
log_line dev INFO "Coolify API: http://$COOLIFY_API_BIND"
log_line dev INFO "scheduler: grpc://$SCHEDULER_GRPC_BIND + unix://$SCHEDULER_UNIX_SOCKET_PATH"
log_line dev INFO "fake coold: host_id=$FAKE_COOLD_HOST_ID caps=$FAKE_COOLD_CAPS"

export ROOT JWT JWT_PUB SCHEDULER_GRPC_BIND SCHEDULER_UNIX_SOCKET_PATH COOLIFY_API_BIND COOLIFY_API_DB COOLIFY_UI_PORT

start_service "scheduler" 'exec env SCHEDULER_GRPC_BIND="$SCHEDULER_GRPC_BIND" SCHEDULER_UNIX_SOCKET_PATH="$SCHEDULER_UNIX_SOCKET_PATH" SCHEDULER_JWT_PUBLIC_KEY_PATH="$JWT_PUB" SCHEDULER_ALLOW_PUBLIC_BIND=1 rtk cargo watch -w scheduler -w proto -w Cargo.toml -w Cargo.lock -i target -x "run -p scheduler"'
wait_for_tcp "$(host_part "$SCHEDULER_GRPC_BIND")" "$(port_part "$SCHEDULER_GRPC_BIND")" "scheduler gRPC"
wait_for_socket "$SCHEDULER_UNIX_SOCKET_PATH" "scheduler UDS"

start_service "fake coold" 'while true; do env SCHEDULER_URL="http://$SCHEDULER_GRPC_BIND" JWT="$JWT" rtk cargo run -p scheduler --example fake_coold || true; sleep 1; done'
start_service "api" 'exec env SKIP_UI=1 COOLIFY_API_BIND="$COOLIFY_API_BIND" COOLIFY_API_DB="$COOLIFY_API_DB" COOLIFY_SCHEDULER_SOCKET="$SCHEDULER_UNIX_SOCKET_PATH" rtk cargo watch -w core -w storage -w api -w migrations -w Cargo.toml -w Cargo.lock -i target -x "run -p api -- serve"'
start_service "ui" 'cd "$ROOT/coolify-ui" && exec env COOLIFY_UI_PORT="$COOLIFY_UI_PORT" bun run dev'

wait_for_tcp "$(host_part "$COOLIFY_API_BIND")" "$(port_part "$COOLIFY_API_BIND")" "api"
check_children

log_line dev INFO "Dev stack is up. Open http://127.0.0.1:$COOLIFY_UI_PORT"
log_line dev INFO "Tip: Servers → Sync scheduler streams → open host-local."

while true; do
  check_children
  sleep 1
done
