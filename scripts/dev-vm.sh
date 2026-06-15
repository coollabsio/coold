#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTANCE="${COOLIFY_COOLD_LIMA_INSTANCE:-coold-dev}"
YAML="$ROOT/dev/lima/coold.yaml"
GUEST_ROOT="/workspace/coold"

usage() {
  cat <<USAGE
Usage: scripts/dev-vm.sh <command>

Commands:
  up       Create/start the Lima VM
  dev      Start the full real-coold dev stack inside the VM
  shell    Open a shell inside the VM
  status   Show Lima instance status
  stop     Stop the VM
  delete   Delete the VM and all VM-local runtime state

Environment:
  COOLIFY_COOLD_LIMA_INSTANCE  Override Lima instance name (default: coold-dev)
USAGE
}

require_lima() {
  command -v limactl >/dev/null 2>&1 || {
    echo "limactl is required. Install Lima first: brew install lima" >&2
    exit 1
  }
}

instance_exists() {
  limactl list 2>/dev/null | awk 'NR > 1 {print $1}' | grep -qx "$INSTANCE"
}

start_vm() {
  if instance_exists; then
    limactl start "$INSTANCE"
  else
    limactl start --tty=false --name="$INSTANCE" "$YAML"
  fi
}

cmd="${1:-}"
case "$cmd" in
  up)
    require_lima
    start_vm
    ;;
  dev)
    require_lima
    start_vm >/dev/null
    exec limactl shell "$INSTANCE" -- bash -lc "export PATH=\"\$HOME/.cargo/bin:\$HOME/.bun/bin:\$PATH\"; cd '$GUEST_ROOT' && REAL_COOLD=1 bun run dev"
    ;;
  shell)
    require_lima
    exec limactl shell "$INSTANCE"
    ;;
  status)
    require_lima
    exec limactl list "$INSTANCE"
    ;;
  stop)
    require_lima
    exec limactl stop "$INSTANCE"
    ;;
  delete|destroy)
    require_lima
    exec limactl delete --force --tty=false "$INSTANCE"
    ;;
  -h|--help|help|"")
    usage
    ;;
  *)
    echo "unknown command: $cmd" >&2
    usage >&2
    exit 1
    ;;
esac
