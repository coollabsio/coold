#!/usr/bin/env sh
set -eu

if [ "${1#-}" != "$1" ]; then
  set -- /usr/local/bin/scheduler "$@"
fi

if [ "$(id -u)" = "0" ]; then
  if [ -n "${SCHEDULER_UNIX_SOCKET_GROUP_ID:-}" ]; then
    group_name="${SCHEDULER_UNIX_SOCKET_GROUP:-coolify-scheduler}"
    if ! getent group "${group_name}" >/dev/null 2>&1; then
      groupadd --gid "${SCHEDULER_UNIX_SOCKET_GROUP_ID}" "${group_name}"
    fi
    usermod -a -G "${group_name}" scheduler
    export SCHEDULER_UNIX_SOCKET_GROUP="${group_name}"
  fi

  mkdir -p /run/coolify /etc/coolify
  chown scheduler:scheduler /run/coolify

  if [ "${SCHEDULER_RUN_AS_ROOT:-0}" != "1" ]; then
    exec setpriv --reuid=scheduler --regid=scheduler --init-groups "$@"
  fi
fi

exec "$@"
