#!/usr/bin/env bash
# Mirror of corrosion's arm64 docker build pattern.
set -euo pipefail

CONTAINER=coold-builder
IMAGE=rust:1.89-bookworm
PLATFORM=linux/arm64
REGISTRY_VOL=coold-cargo-registry
GIT_VOL=coold-cargo-git
TARGET_VOL=coold-cargo-target-arm64
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARTIFACT_DIR="$REPO_ROOT/target/arm64/release"

cmd="${1:-build}"

container_running() {
  [ "$(docker inspect -f '{{.State.Running}}' "$CONTAINER" 2>/dev/null || true)" = "true" ]
}

container_exists() {
  docker inspect "$CONTAINER" >/dev/null 2>&1
}

container_has_target_volume() {
  [ "$(docker inspect -f '{{range .Mounts}}{{if and (eq .Destination "/cargo-target") (eq .Name "'"$TARGET_VOL"'")}}true{{end}}{{end}}' "$CONTAINER" 2>/dev/null || true)" = "true" ]
}

require_running_container() {
  if container_running; then
    return
  fi
  docker logs "$CONTAINER" >&2 2>/dev/null || true
  echo "container $CONTAINER exited immediately; ensure Docker can run $PLATFORM containers (qemu/binfmt)" >&2
  exit 1
}

start() {
  if container_exists && ! container_has_target_volume; then
    docker rm -f "$CONTAINER" >/dev/null
    echo "container $CONTAINER recreated for isolated target cache"
  fi
  if container_running; then
    echo "container $CONTAINER already running"
    return
  fi
  if container_exists; then
    docker start "$CONTAINER" >/dev/null
    require_running_container
    echo "container $CONTAINER resumed"
    return
  fi
  docker run -d --name "$CONTAINER" \
    --platform "$PLATFORM" \
    -v "$REPO_ROOT":/src -w /src \
    -v "$REGISTRY_VOL":/usr/local/cargo/registry \
    -v "$GIT_VOL":/usr/local/cargo/git \
    -v "$TARGET_VOL":/cargo-target \
    -e CARGO_TARGET_DIR=/cargo-target \
    "$IMAGE" sleep infinity >/dev/null
  require_running_container
  echo "installing clang + mold..."
  docker exec "$CONTAINER" bash -c "apt-get update -qq && apt-get install -y -qq clang mold >/dev/null"
  echo "container $CONTAINER ready"
}

build() {
  start
  trap 'docker stop "$CONTAINER" >/dev/null 2>&1 || true; echo "container $CONTAINER stopped"' EXIT
  docker exec -e CARGO_TARGET_DIR=/cargo-target "$CONTAINER" cargo build --release
  mkdir -p "$ARTIFACT_DIR"
  docker cp "$CONTAINER":/cargo-target/release/coold "$ARTIFACT_DIR/coold"
  echo "binary: $ARTIFACT_DIR/coold"
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
  docker volume rm "$REGISTRY_VOL" "$GIT_VOL" "$TARGET_VOL" 2>/dev/null || true
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
