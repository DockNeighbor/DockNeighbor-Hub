# The hub on a Raspberry Pi, and in a container

Owner rulings, Jonathan 2026-10-05: **"pi should run full hub only, docker is fine"**, **"64-bit
only"**, **"yes refuse updates in docker"**. A Pi is not a constrained router — it runs the *full*
daemon, with the cycle machine, the volume cap and the flood handover, rather than the POSIX-shell
hub-lite's reimplementation of the code that stops a flood.

## 🔴 64-bit only

There is **no armv7 build** and there will not be one. Raspberry Pi OS still ships a 32-bit image
for older installs; a Pi 3, 4 or 5 all run the 64-bit one, and that is what the hub needs.

This fails *safely* rather than confusingly: the daemon's updater refuses an architecture it has no
asset for (`self_update::asset_for` returns `None`) instead of guessing, so a 32-bit install does
not quietly fetch the wrong binary — it declines.

Check before you start:

```bash
uname -m   # aarch64 = good · armv7l = 32-bit, reinstall with the 64-bit image
```

## Native (systemd)

The release already publishes a statically linked `brvg-hub-linux-arm64` — nothing to compile.

```bash
curl -fsSL -o brvg-hub https://github.com/DockNeighbor/DockNeighbor-Hub/releases/latest/download/brvg-hub-linux-arm64
sudo install -m 0755 brvg-hub /usr/local/bin/brvg-hub
sudo cp daemon/linux/brvg-hub.service /etc/systemd/system/
sudo systemctl enable --now brvg-hub
```

State lives in `/var/lib/DockNeighbor/hub.json` — the vessel id and the hub's bearer token. Back
that up, or re-enrolling creates a second device against the vessel.

A natively installed hub **does** self-update, subject to the owner's switch and the quiet window.

## Container

```bash
docker run -d --name dockneighbor-hub \
  --network host \
  -e TZ=America/New_York \
  -v dockneighbor-hub:/var/lib/DockNeighbor \
  ghcr.io/dockneighbor/dockneighbor-hub:latest
```

- **`--network host`** so LAN devices (Shellys, LinkTap, routers) can reach the hub on **8722** and
  so the hub can reach them. On a locked-down network use `-p 8722:8722` instead and accept that
  device discovery on the LAN will not work.
- **The volume is not optional.** Without it, re-creating the container re-enrols the hub and the
  owner sees a second device appear against their boat.
- **Set `TZ`.** The quiet window is 02:00–05:00 *local*; a container with no zone set answers UTC,
  which on a boat is simply a different night. `tzdata` is in the image; the zone is not.
- **No CA bundle is needed or used.** The daemon's TLS is rustls with webpki roots compiled into
  the binary. A root-store rotation therefore arrives with a new image, not an OS package.

### 🔴 A containerised hub never self-updates

`may_update` refuses with `Blocked::Containerised` before it even reads the owner's switch. A
container is replaced by pulling a new image, never by swapping the binary inside a running one:
an in-place update would be silently reverted on the next pull, while the probation watcher counted
a rollback deadline against a filesystem about to be discarded.

So **you** update it:

```bash
docker pull ghcr.io/dockneighbor/dockneighbor-hub:latest
docker stop dockneighbor-hub && docker rm dockneighbor-hub
# ...then the same `docker run` as above; the volume carries the enrolment across.
```

Pin the version tag rather than `latest` if you would rather decide when that happens.

## How the image is built

`daemon/docker/Dockerfile` **copies in the released binary**; it does not rebuild it. The image
carries the exact artifact the release published, because two build paths for one binary is how
they stop being the same binary. `.github/workflows/daemon-image.yml` runs after a release is
published, downloads that release's own assets, and pushes `linux/amd64` + `linux/arm64` to GHCR —
deliberately separate from `daemon-release.yml`, so a publishing problem can never fail a release
that every hub in the field updates through.
