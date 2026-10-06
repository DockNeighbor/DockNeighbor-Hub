# syntax=docker/dockerfile:1
#
# THE DOCKNEIGHBOR HUB DAEMON AS A CONTAINER (owner ruling, Jonathan 2026-10-05: a Pi runs the full
# hub, "docker is fine"). Multi-arch: linux/amd64 and linux/arm64.
#
# 🔴 THE BINARY IS COPIED IN, NOT REBUILT HERE. It is the SAME statically linked musl artifact the
# release publishes (`brvg-hub-linux-x64` / `brvg-hub-linux-arm64`), staged into `dist/` by
# .github/workflows/daemon-image.yml. Building it a second way inside this file would be a second
# build path for one binary, which is how two things that must be identical stop being identical.
#
# 🔴 NO ca-certificates, DELIBERATELY. The daemon speaks TLS through rustls with webpki-roots
# COMPILED IN (reqwest `rustls-tls`, tokio-tungstenite `rustls-tls-webpki-roots`) — there is no
# OpenSSL and no system trust store in the path. Adding a CA bundle here would imply the daemon
# reads one, which it does not. The corollary worth knowing: a root-store rotation ships with a new
# binary, not with an OS package.
#
# 32-BIT IS NOT BUILT (owner ruling 2026-10-05: "64-bit only"). A Raspberry Pi must run a 64-bit OS.
# The daemon's own updater already refuses an arch it has no asset for rather than guessing.

FROM alpine:3.21

# tzdata ONLY. The hub resolves a LOCAL hour (the quiet window is 02:00–05:00 local), and a
# container with no zone database silently answers UTC — which on a boat is simply a different
# night. Set TZ when you run it.
RUN apk add --no-cache tzdata \
 && addgroup -S dockneighbor \
 && adduser -S -G dockneighbor -H -s /sbin/nologin dockneighbor \
 && mkdir -p /var/lib/DockNeighbor \
 && chown dockneighbor:dockneighbor /var/lib/DockNeighbor

# buildx sets TARGETARCH per platform: amd64 | arm64.
ARG TARGETARCH
COPY --chmod=0755 dist/brvg-hub-linux-${TARGETARCH} /usr/local/bin/brvg-hub

USER dockneighbor

# The hub's state: /var/lib/DockNeighbor/hub.json holds the vessel id and the hub's bearer token
# (hub_config.rs `shared_base` + `DIR_NAME`). MOUNT IT — without a volume, re-creating the
# container re-enrols the hub and the owner sees a second device.
VOLUME ["/var/lib/DockNeighbor"]

# The LAN device API and the app's door (hub_config::DEFAULT_HTTP_PORT).
EXPOSE 8722

ENTRYPOINT ["/usr/local/bin/brvg-hub"]
