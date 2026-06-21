# TODO

## Remaining primitive implementation

- Add volume primitives:
  - `volumes.create`
  - `volumes.inspect`
  - `volumes.delete`
- Add network primitives:
  - `networks.create`
  - `networks.list`
  - `networks.delete`
- Add firewall primitives with dual-plane persistence:
  - `firewall.allow`
  - `firewall.revoke`
  - `firewall.list`
  - `firewall.reconcile`
  - persist `/etc/coolify/allow.rules` and `/etc/coolify/allow.nft` atomically
- Add explicit service discovery primitives:
  - `services.register`
  - `services.unregister`
  - `services.endpoints`
  - decide how these coexist with the current Podman-event Corrosion sync loop
- Add DNS diagnostic primitives:
  - `dns.lookup`
  - `dns.stats`
- Add host facts primitives:
  - `host.info`
  - `host.stats`
  - `host.containers`

## Deferred real-engine smoke tests

- Add ignored real-Podman primitive smoke tests against the Coolify-managed Linux VM for the image/container primitive surface:
  - `images.pull`, `images.list`, `images.delete`
  - `containers.create`, `containers.start`, `containers.list`, `containers.inspect`, `containers.logs`, `containers.exec`, `containers.stop`, `containers.delete`
  - one negative `containers.create` deny-filter case, such as `privileged: true`
- Add real-Podman smoke coverage for each remaining primitive group as it is implemented.
