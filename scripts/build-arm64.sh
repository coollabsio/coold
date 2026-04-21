#!/usr/bin/env bash
# Mirror of corrosion's arm64 docker build pattern.
set -euo pipefail

CONTAINER=coold-builder
IMAGE=rust:1.89-bookworm
PLATFORM=linux/arm64
REGISTRY_VOL=coold-cargo-registry
GIT_VOL=coold-cargo-git
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cmd="${1:-build}"

container_running() {
  [ "$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" = "true" ]
}

container_exists() {
  docker inspect "$CONTAINER" >/dev/null 2>&1
}

start() {
  if container_running; then
    echo "container $CONTAINER already running"
    return
  fi
  if container_exists; then
    docker start "$CONTAINER" >/dev/null
    echo "container $CONTAINER resumed"
    return
  fi
  docker run -d --name "$CONTAINER" \
    --platform "$PLATFORM" \
    -v "$REPO_ROOT":/src -w /src \
    -v "$REGISTRY_VOL":/usr/local/cargo/registry \
    -v "$GIT_VOL":/usr/local/cargo/git \
    "$IMAGE" sleep infinity >/dev/null
  echo "installing clang + mold..."
  docker exec "$CONTAINER" bash -c "apt-get update -qq && apt-get install -y -qq clang mold >/dev/null"
  echo "container $CONTAINER ready"
}

build() {
  start
  trap 'docker stop "$CONTAINER" >/dev/null 2>&1 || true; echo "container $CONTAINER stopped"' EXIT
  docker exec "$CONTAINER" cargo build --release
  echo "binary: $REPO_ROOT/target/release/coold"
}

shell_cmd() {
  start
  docker exec -it "$CONTAINER" bash
}

stop_cmd() {
  if container_exists; then
    docker rm -f "$CONTAINER" >/dev/null
    echo "container removed (volumes kept)"
  fi
}

clean() {
  stop_cmd
  docker volume rm "$REGISTRY_VOL" "$GIT_VOL" 2>/dev/null || true
  echo "volumes removed"
}

case "$cmd" in
  start) start ;;
  build) build ;;
  shell) shell_cmd ;;
  stop)  stop_cmd ;;
  clean) clean ;;
  *) echo "usage: $0 {start|build|shell|stop|clean}"; exit 1 ;;
esac
