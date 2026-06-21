# TODO

- Add ignored real-Podman primitive smoke tests against the Lima VM for the image/container primitive surface:
  - `images.pull`, `images.list`, `images.delete`
  - `containers.create`, `containers.start`, `containers.list`, `containers.inspect`, `containers.logs`, `containers.exec`, `containers.stop`, `containers.delete`
  - one negative `containers.create` deny-filter case, such as `privileged: true`
