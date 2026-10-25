# plugged.host Wings

plugged.host's node daemon: [Calagopus Wings](https://github.com/calagopus/wings) (wings-rs), the
counterpart of plugged.host's panel ([plugged-host/panel](https://github.com/plugged-host/panel)).
plugged.host's changes go directly into this source, like the panel's; this folder holds what is only
plugged.host's: the build profile's scripts, the service and this guide.

## What the fork changes

Every changed upstream line carries a `plugged.host` comment, every new file a `plugged.host:` header
(`git grep -n "plugged.host"`). The panel's [`docs/wings.md`](https://github.com/plugged-host/panel/blob/main/docs/wings.md)
says which panel feature needs which addition.

| Change | Where |
|---|---|
| `GET /api/public/resources` without a token, for any origin: memory, disk (the data directory's filesystem) and CPU as total, used, free and allocated to servers. `api.public_resources: false` turns it off | `routes/api/public/`, `config.rs`, `main.rs` (`handle_cors`), `stats.rs` (accessors) |
| `GET /api/system/load`, `GET /api/system/processes`, `GET /api/servers/{server}/processes`: load, and every container's processes and connections from `/proc` (for the panel's abuse protection; replaces a separate node agent) | `processes.rs`, `routes/api/system/{load,processes}.rs`, `routes/api/servers/_server_/processes.rs` |
| `POST /api/system/orphans`: removes what a server the panel moved away by node failover left on this node (the panel sends its configuration, which this node no longer gets) | `routes/api/system/orphans.rs` |
| `build.cpu_weight` in a server's configuration: the container's CPU shares, set on create and live on every sync, so the panel can raise the CPU limit for burst cores while paid CPU keeps the weight; `build.cpu_burst`: the borrowed part of the CPU limit, left out of `/api/public/resources`' allocated CPU | `server/configuration/mod.rs`, `server/executor/docker/mod.rs`, `routes/api/public/resources.rs` |
| A schedule's restart step only restarts a running server | `server/schedule/actions.rs` |
| A `.jar` is browsed like a `.zip` (`files/contents` on `/plugins/x.jar/plugin.yml`), so the panel reads a plugin's or mod's descriptor without downloading the jar | `server/filesystem/mod.rs` |
| Build profile `release-fast` (no LTO, incremental) | `Cargo.toml` |

## Build and deploy

Never build on the panel box. `build.sh` copies the tree to the build host (ssh alias `plugged-build`,
the box that builds the panel), builds the `release-fast` profile (no LTO, incremental, see
`Cargo.toml`) and fetches the binary. `wings-rs version` names the commit (`1.2.5:<commit>@main`).
It builds inside a Debian 12 container (`BUILD_IMAGE`, default `rust:1.99-bookworm`), so the binary
needs glibc 2.34 at most and also runs on Debian 12 nodes; `BUILD_IMAGE=` builds on the host instead.

```bash
plugged-host/check.sh                 # rustfmt (files copied back), clippy, tests, on the build host
git commit … && git push              # the binary names the commit
plugged-host/build.sh                 # -> target/plugged-host/wings-rs
plugged-host/deploy.sh <node>         # -> /usr/local/bin/calagopus-wings, restart, health check, rollback
```

Server containers keep running across a restart. The node's config has
`ignore_panel_wings_upgrades: true`, so the panel never swaps this build for an upstream release.

## A node next to Pelican Wings

A node can run this next to Pelican's Wings (nl1 does, for the Pelican dev panel). Nothing is shared:

| | Pelican Wings | Calagopus Wings |
|---|---|---|
| Unit, binary | `wings`, `/usr/local/bin/wings` | `calagopus-wings` (`calagopus-wings.service`), `/usr/local/bin/calagopus-wings` |
| Config | `/etc/pelican/config.yml` | `/etc/calagopus-wings/config.yml`, pinned with `--config` |
| Data | `/var/lib/pelican` | `/var/lib/calagopus-wings` |
| API, SFTP | 8080, 2022 | 8443, 2023 |
| Docker network | `pelican_nw`, 172.18.0.0/16, `fdba:17c8:6c94::/64` | `calagopus_nw`, 172.19.0.0/16, `fdba:17c8:6c95::/64` |
| Game ports | the Pelican panel's allocations | the panel's allocations, a range Pelican does not deploy to |

Always pass `--config /etc/calagopus-wings/config.yml`: without it wings-rs falls back to
`/etc/pelican/config.yml` once its own is missing, and `configure` would rewrite Pelican's config.

Setting one up:

1. In the panel (Admin › Nodes): a node with URL `https://<fqdn>:8443` and SFTP port 2023, then
   allocations on `0.0.0.0` with the node's FQDN as alias, in a port range the other panel does not
   use. Create an enrollment code on the node.
2. On the node, as root:

   ```bash
   install -m 755 wings-rs /usr/local/bin/calagopus-wings
   install -d -m 700 /etc/calagopus-wings
   calagopus-wings --config /etc/calagopus-wings/config.yml configure --enroll <code> --panel-url https://calagopus.plugged.host
   ```

3. In `/etc/calagopus-wings/config.yml`: `api.ssl` (`enabled: true`, the node's certificate, e.g.
   `/etc/letsencrypt/live/<fqdn>/{fullchain,privkey}.pem`), the Docker network's
   `interface`/`interfaces.v4`/`interfaces.v6` on the subnets above, and
   `ignore_panel_wings_upgrades: true`.
4. `calagopus-wings.service` into `/etc/systemd/system/`, then
   `systemctl daemon-reload && systemctl enable --now calagopus-wings`.

## Keeping up with upstream

```bash
git fetch upstream && git merge upstream/main      # on main; never a side branch
```
