# plugged.host Wings

plugged.host's node daemon: [Calagopus Wings](https://github.com/calagopus/wings) (wings-rs), the
counterpart of plugged.host's panel ([plugged-host/panel](https://github.com/plugged-host/panel)).
Every game node runs it. plugged.host's changes go directly into this source, like the panel's; this
folder holds what is only plugged.host's: the build and deploy scripts, the service and this guide.

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
| Burst operations: a server with burst CPU (`build.cpu_burst`, read live) gets `1 + cpu_burst / 100` times the configured threads, up to the node's cores (0 stays all cores), to compress and decompress (routes and schedule steps) and for the wings, s3, ddup-bak and pbs backups; its `restic backup` gets `RESTIC_READ_CONCURRENCY` (2 boosted the same way, unless the repository's environment sets it). A server without burst CPU runs `restic backup` at nice +10 (restores untouched); unlimited-CPU servers are left as upstream. `api.burst_operations: false` turns it off | `server/fast_lane.rs`, `config.rs`, `routes/api/servers/_server_/files/{compress,decompress}.rs`, `server/schedule/actions.rs`, `server/backup/adapters/{wings,s3,ddup_bak,pbs,restic}.rs` |
| ZIP entries of 64 MiB and more that are not stored as incompressible are deflated pigz-style in 1 MiB chunks on the archive's pool (each primed with the 32 KiB before it, ended with a sync flush), for every server whenever the pool has more than one thread; the ZipWriter still writes the headers, zip64 fields and central directory. Streamed zips (directory downloads, btrfs/zfs backups downloaded as zip) keep the one-thread path | `server/filesystem/archive/create/{zip,zip_deflate}.rs` |
| Build profile `release-fast` (no LTO, incremental) | `Cargo.toml` |

## On a node

| What | Where |
|---|---|
| Unit | `calagopus-wings` ([`calagopus-wings.service`](calagopus-wings.service)) |
| Binary | `/usr/local/bin/calagopus-wings`, the previous one kept as `calagopus-wings.previous` |
| Config | `/etc/calagopus-wings/config.yml` (it holds the node's panel token: root only, never in git) |
| Logs | `journalctl -u calagopus-wings` |

Always pass `--config /etc/calagopus-wings/config.yml` (the unit does): without it wings-rs falls
back to `/etc/pelican/config.yml` when its own default is missing. The config has
`ignore_panel_wings_upgrades: true`, so the panel never swaps this build for an upstream release.

The nodes that ran Pelican's Go Wings until 2026-10-27 were switched to this fork in place: their
config keeps the old data directory, Docker network, ports and SFTP host key. The old `wings` unit
stays disabled there; never start it, it would fight this one over the same ports and containers.

## Build and deploy

Never build on the panel's webserver box. `build.sh` copies the tree to the build host (ssh alias
`BUILD_HOST`, default `plugged-build`), builds the `release-fast` profile and fetches the binary.
It builds inside a Debian 12 container (`BUILD_IMAGE`, default `rust:1.99-bookworm`), so the binary
needs glibc 2.34 at most and also runs on Debian 12 nodes; `BUILD_IMAGE=` builds on the host instead.
`wings-rs version` names the commit (`<upstream version>:<commit>@main`).

```bash
cd /var/www/plugged.host/wings
plugged-host/check.sh                 # rustfmt (files copied back), clippy, tests, on the build host
git commit … && git push              # the binary names the commit
plugged-host/build.sh                 # -> target/plugged-host/wings-rs
plugged-host/deploy.sh <node>         # one node, by its ssh alias
```

`deploy.sh` copies the binary to the node, keeps the old one as `calagopus-wings.previous`,
restarts the unit and waits for the API (a 401 without a token means it is listening). If it does
not come up, it puts the previous binary back. Server containers keep running across the restart
(`KillMode=process`). Deploy one node first, check it in the panel (Admin › Nodes), then the rest.
The node list (ssh aliases) is in plugged.host's private ops docs.

**Roll back** by hand on the node:

```bash
cp -p /usr/local/bin/calagopus-wings.previous /usr/local/bin/calagopus-wings
systemctl restart calagopus-wings
```

Or deploy an older build: check out its commit, `build.sh`, `deploy.sh <node>`.

## A new node

1. In the panel (Admin › Nodes): the node in its location, role `internal` until it is ready, URL
   `https://<fqdn>:<api port>` and its SFTP port; allocations on `0.0.0.0` with the node's FQDN as
   alias. Create an enrollment code on the node.
2. On the node, as root, with Docker installed:

   ```bash
   install -m 755 wings-rs /usr/local/bin/calagopus-wings
   install -d -m 700 /etc/calagopus-wings
   calagopus-wings --config /etc/calagopus-wings/config.yml configure --enroll <code> --panel-url https://<panel>
   ```

3. In `/etc/calagopus-wings/config.yml`: `api.ssl` (`enabled: true`, the node's certificate, e.g.
   `/etc/letsencrypt/live/<fqdn>/{fullchain,privkey}.pem`), `ignore_panel_wings_upgrades: true`,
   and the data directory (`system.data`) under `allowed_mounts` (devboxes mount their game
   server's volume).
4. `calagopus-wings.service` into `/etc/systemd/system/`, then
   `systemctl daemon-reload && systemctl enable --now calagopus-wings`.
5. Check: the node's page in Admin › Nodes answers, and
   `curl https://<fqdn>:<api port>/api/public/resources` answers without a token. Then role `game`.

The node baseline (SSH, firewall, OOM protection, backups) is in plugged.host's private ops docs.

## Releases

GitHub releases follow the panel's version (`v3.0.0`), cut only when Wings changed:
`./tag-release.sh 3.0.4` pushes the tag `v3.0.4`, the Release workflow builds the binaries and the
images (`ghcr.io/plugged-host/wings`) and leaves a draft release to publish. The panel's release
notes name the Wings release that goes with it.

## Keeping up with upstream

```bash
git fetch upstream && git merge upstream/main      # on main; never a side branch
```
