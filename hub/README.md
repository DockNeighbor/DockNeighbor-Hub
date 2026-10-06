> **FROZEN — no new capability (owner ruling 2026-08-19).** The project converged on the Rust
> daemon (`../daemon`) as the one full-hub implementation for every capable host — desktop and
> containers alike. This TypeScript hub keeps working as-is but stops growing: new capability goes
> in the daemon, and this implementation's LinkTap client and cycle machine are the porting source
> (with their fixtures) for the daemon's LinkTap work. `hub-lite/` (hub-lite) is unaffected — it is a
> tier for constrained routers, not a duplicate.

# BRVG hub (TypeScript) — Pi / Docker / desktop

The **hub tier** of the on-site architecture of DockNeighbor. It receives the
Shellys' webhooks on the LAN and rolls them up into ONE `/api/agent/batch` report per interval —
the same wire contract the shell hub-lite on a GL.iNet speaks, and the same the worker validates.

**Why TypeScript, and why a hub rather than the router hub-lite** (owner, 2026-08-13): a capable box
(a Pi, a Docker host, the desktop app) can host far more than the 416 KB of writable flash on a
GL-X750 allows. The hub is the growth path — aggregation today, provisioning and configuration
next — and living in TypeScript means it can share the app's existing Shelly code instead of a
second implementation drifting from the first. On the constrained routers the shell hub-lite stays;
**a hub-lite is a subset of a hub, never a variant**, which is why both are tested against the one
canonical batch fixture.

## What it does today

- Listens on `:8181` (`/cgi-bin/report`, same path as the shell hub-lite) for Shelly webhooks.
- Spools telemetry (`*.measurement` / `*.change`) and drains one roll-up per interval, with an
  unchanged device riding the `ok` list instead of re-sending its reading.
- Sends **alarms immediately, on their own** — aggregation never delays one.
- Retries a failed drain under the **same sequence number**, so the worker drops a replay whole
  instead of re-firing alerts.
- **GPS, two opt-in sources**, both spooling `gps.measurement` under the hub's own device id:
  - **Cradlepoint by HTTP poll** (`CRADLEPOINT_HOST`): `GET /api/status/gps` with Basic auth every
    `GPS_INTERVAL` — the hub POLLS the router (owner ruling 2026-08-17); nothing is configured on
    the router to send anywhere. A stationary vessel dedups via the roll-up's unchanged signature.
  - **NMEA 0183 over TCP** (`NMEA_HOST`): dials a LAN stream — chartplotter, AIS, gpsd — parses
    RMC/GGA with the app's parser and throttles the 1–10 Hz stream to movement ≥ 25 m /
    stationary heartbeats. The hub is always the client; nothing ever listens on the WAN.
- Zero runtime dependencies (Node 18+ built-in `http`/`net` + global `fetch`).

Not yet: on-hub provisioning / device configuration. The open design question is whether it starts
read-only.

## Run it

Config is environment variables (enroll the hub in the app to mint `DEVICE_TOKEN`):

| var | required | default |
| --- | --- | --- |
| `VID` | ✓ | — |
| `DEVICE_ID` | ✓ | — |
| `DEVICE_TOKEN` | ✓ | — |
| `WORKER_URL` | | `https://api.dockneighbor.com` (must be https) |
| `RECEIVER_PORT` | | `8181` |
| `DRAIN_INTERVAL` | | `120` (floor 30) |
| `KEYFRAME_EVERY` | | `6` |
| `NMEA_HOST` | | — (empty = NMEA source off) |
| `NMEA_PORT` | | `10110` |
| `CRADLEPOINT_HOST` | | — (empty = Cradlepoint poll off) |
| `CRADLEPOINT_PORT` | | `80` |
| `CRADLEPOINT_USER` | | `admin` |
| `CRADLEPOINT_PASSWORD` | | — |
| `GPS_INTERVAL` | | `120` (floor 30) — poll / stationary-report cadence, same name as the hub-lite's |

**Docker — RETIRED (owner ruling, Jonathan 2026-10-05).** This implementation's `Dockerfile` has
been removed. A Pi or a container runs the **full Rust daemon**, which is the one full-hub
implementation for every capable host (the 2026-08-19 convergence ruling that froze this tier, and
the 2026-10-05 ruling that settled the Pi): see [`../daemon/docker/`](../daemon/docker/). Leaving a
Dockerfile here was an invitation to containerise the frozen implementation by accident.

The daemon's image is 64-bit only (`linux/amd64`, `linux/arm64`) — there is no armv7 build, by
ruling. A containerised hub also refuses to self-update: you replace it by pulling a new image.

**Pi / bare Linux** (systemd):

```sh
sudo mkdir -p /opt/brvg-hub && sudo cp -r dist /opt/brvg-hub/
printf 'VID=…\nDEVICE_ID=…\nDEVICE_TOKEN=…\n' | sudo tee /etc/brvg-hub.env >/dev/null
sudo chmod 600 /etc/brvg-hub.env
sudo cp brvg-hub.service /etc/systemd/system/ && sudo systemctl enable --now brvg-hub
```

Then, in the app, use **Route sensors through this router/hub** so the Shellys point their webhooks
at the hub instead of the cloud.

## Health

`GET /healthz` → `{ ok, spooled, tier: "hub", version }`. Handy for a container healthcheck.

## Develop

```sh
npm install
npm test          # vitest — contract, aggregator, receiver, and an in-process server test
npm run build     # tsc → dist/
```
