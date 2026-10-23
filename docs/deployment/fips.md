# FIPS mesh

[FIPS](https://github.com/jmcorgan/fips) is a mesh network whose nodes are
addressed by Nostr keys. On the mesh, Almond can at the same time:

- serve over normal HTTP(S) via Docker port publishing,
- serve the same instance to peered FIPS nodes at `http://<this-node-npub>.fips:3000`,
- fetch from upstream Blossom servers reachable only inside the mesh.

`ghcr.io/flox1an/almond-fips` contains FIPS, dnsmasq and Almond. There are two
ways to run it:

| | Pod (recommended) | Single container |
|---|---|---|
| Containers | `pause`, `almond-fips` running only FIPS, standard Almond image | `almond-fips` running FIPS and Almond |
| Almond runs as | UID `10001`, no capabilities | root, with `NET_ADMIN` |
| FIPS node key | only in the FIPS container | next to Almond |
| FIPS crash | Almond keeps running; the mesh is back after the restart | the container restarts |
| Start with | [Docker Compose](#docker-compose) | [`docker run`](#single-container) |

**Requirements.** The container running FIPS needs `NET_ADMIN` and the host TUN
device (`/dev/net/tun`): its entrypoint creates the virtual IPv6 interface
(usually `fips0`) and installs small DNS and iptables rules. If `/dev/net/tun`
is missing on a Linux host, run `sudo modprobe tun`. Hosting providers that
reject `devices`, `cap_add` or `sysctls` (or run containers under gVisor or Kata)
cannot run FIPS in a container; run FIPS on the host instead, or use a VPS or
bare-metal server.

All `FIPS_*` variables are listed in the
[configuration reference](/configuration/environment#fips-mesh).

## Docker Compose

The bundled
[`docker-compose.fips.yml`](https://github.com/flox1an/almond/blob/main/docker-compose.fips.yml)
runs the pod:

```yaml
services:
  pause:
    image: registry.k8s.io/pause:3.10
    restart: unless-stopped
    sysctls:
      - net.ipv6.conf.all.disable_ipv6=0
    ports:
      - "3000:3000"
      - "2121:2121/udp"

  fips:
    image: ghcr.io/flox1an/almond-fips:main
    network_mode: service:pause
    restart: unless-stopped
    cap_add:
      - NET_ADMIN
    devices:
      - /dev/net/tun:/dev/net/tun
    environment:
      FIPS_RUN_ALMOND: "false"
      FIPS_NSEC: ${FIPS_NSEC:?set FIPS_NSEC}
      FIPS_PEERS: ${FIPS_PEERS:-}
      FIPS_HOSTS: ${FIPS_HOSTS:-}
      # ... single-peer variables, see the file

  almond:
    image: ghcr.io/flox1an/almond:main
    network_mode: service:pause
    restart: unless-stopped
    depends_on:
      - fips
    cap_drop:
      - ALL
    security_opt:
      - no-new-privileges:true
    environment:
      ALMOND_BIND_ADDR: "[::]:3000"
      ALMOND_PUBLIC_URL: ${ALMOND_PUBLIC_URL:?set ALMOND_PUBLIC_URL}
      ALMOND_UPSTREAM_MODE: ${ALMOND_UPSTREAM_MODE:-proxy}
      ALMOND_UPSTREAM_SERVERS: ${ALMOND_UPSTREAM_SERVERS:-}
    volumes:
      - almond-files:/app/files

volumes:
  almond-files:
```

Copy
[`.env.fips.example`](https://github.com/flox1an/almond/blob/main/.env.fips.example)
to `.env.fips`, fill in your node and start the pod:

```dotenv
ALMOND_PUBLIC_URL=http://<this-node-npub>.fips:3000
ALMOND_UPSTREAM_MODE=proxy
ALMOND_UPSTREAM_SERVERS=

FIPS_NSEC=nsec1...
FIPS_PEERS="npub1aaa...,203.0.113.10:2121,gateway-a,udp
npub1bbb...,198.51.100.20:2121,gateway-b,udp"
FIPS_HOSTS=
```

```bash
docker compose --env-file .env.fips -f docker-compose.fips.yml up -d
```

Several peers in `FIPS_PEERS` are recommended in production: Almond keeps
routing when one gateway is unavailable, which matters for large transfers.
Multi-line values must be quoted in the env file. For a single peer, the
`FIPS_PEER_*` variables are an alternative.

Further Almond settings go into the `almond` service's `environment`. Almond
runs as in the standard image, so a host bind mount instead of the named volume
must be owned by `10001:10001`; see [Docker](/deployment/docker).

### How the pod works

```text
 pause  ── owns the network namespace: eth0, published ports
   ├── fips    joins it, creates fips0, runs dnsmasq    NET_ADMIN, TUN, node key
   └── almond  joins it, listens on [::]:3000            no capabilities
```

Docker ties a network namespace to the container that created it. If FIPS owned
the namespace and crashed, Docker would restart it in a new namespace, and
Almond would stay behind with only `lo`: still running, but offline. The `pause`
container (the same image Kubernetes uses for every pod) only sleeps, so it does
not crash. FIPS restarts into the existing namespace and recreates `fips0`;
Almond keeps its network. While FIPS restarts, mesh traffic and DNS resolution
pause for a few seconds.

Consequences for the configuration:

- **Ports** are published on `pause`, because that is where the namespace lives.
- **`ALMOND_BIND_ADDR`** stays on the IPv6 wildcard `[::]`: `fips0` carries IPv6
  only. The wildcard accepts IPv4 on the published port as well.
- **DNS:** all three containers share the `pause` container's
  `/etc/resolv.conf`. The FIPS container points it at its dnsmasq, which sends
  `.fips` names to FIPS and everything else to Docker's DNS, so Almond resolves
  both.
- **`FIPS_ISOLATE=true`** installs its firewall in the shared namespace and
  therefore restricts Almond too: no egress except the mesh, which also cuts off
  public upstream servers.
- **Port 443:** Almond runs as a non-root user. To listen on 443 (so
  `https://<this-node-npub>.fips` works without a port), add the sysctl
  `net.ipv4.ip_unprivileged_port_start=443` to the `pause` service.

## Coolify

Use Coolify's Docker Compose deployment mode with the pod from
`docker-compose.fips.yml`. Coolify keeps `network_mode: service:pause` and does
not attach its own networks to the `fips` and `almond` services.

1. Check that the target server has `/dev/net/tun` (`ls -l /dev/net/tun`); if not,
   run `sudo modprobe tun` on the Linux host.
2. Create a Docker Compose resource and paste `docker-compose.fips.yml`.
3. Write the `FIPS_*` values directly into the `fips` service's `environment`
   instead of `${...}` references, and the Almond settings into the `almond`
   service. Coolify stores every `${...}` variable in one `.env` file that it
   attaches to all services of the resource, which would hand the node key to
   Almond.
4. For public HTTP(S) through Coolify's proxy, assign the domain to the `pause`
   service on container port `3000`: it owns the network namespace. Keep
   `2121:2121/udp` published for the FIPS transport.

Do not rename services of an existing Compose resource: Coolify keeps the old
service as an orphaned entry and reports the resource as degraded.

## Single container

FIPS and Almond run together as root in one container. Bind Almond to all
interfaces so both the published port and the mesh reach it:

```bash
docker run \
  --cap-add NET_ADMIN \
  --device /dev/net/tun:/dev/net/tun \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -p 3000:3000 \
  -p 2121:2121/udp \
  -v /path/to/files:/app/files \
  -e ALMOND_STORAGE_PATH=/app/files \
  -e 'ALMOND_BIND_ADDR=[::]:3000' \
  -e ALMOND_PUBLIC_URL=https://public.example.com \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1gateway... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  ghcr.io/flox1an/almond-fips:main
```

Public clients use `https://public.example.com`; FIPS peers use
`http://<this-node-npub>.fips:3000`. For mesh-only operation, drop the `3000:3000`
mapping and keep the transport port `2121/udp`.

## Using the mesh

The examples use `docker run` flags; in the pod, `ALMOND_*` settings go to the
`almond` service and `FIPS_*` settings to the `fips` service.

**HTTPS inside the mesh** uses Almond's normal TLS settings; peers then use
`https://<this-node-npub>.fips:3000`. The certificate must be valid for the
hostname clients use, or clients must trust it explicitly.

```bash
-e ALMOND_TLS_ENABLED=true \
-e ALMOND_TLS_CERT=/app/certs/cert.pem \
-e ALMOND_TLS_KEY=/app/certs/key.pem \
-v /path/to/certs:/app/certs
```

**Short names.** `FIPS_HOSTS='my-almond npub1thisnode...'` lets peers with the
same mapping use `http://my-almond.fips:3000`. The canonical `<npub>.fips` name
works without aliases.

**Browser access** has different requirements than peered FIPS nodes. FIPS
encrypts and authenticates the mesh connection, but that does not make an
`http://` URL an HTTPS origin: an HTTPS web page cannot `fetch()` an HTTP mesh
URL (mixed content), and images or media may be upgraded to HTTPS and then fail.
The options:

| Option | How | Constraint |
|---|---|---|
| HTTPS gateway | A reverse proxy with a normal certificate serves browsers and reaches Almond by its `.fips` name | Clients need no mesh access, but the gateway sees the content; configure fixed upstreams, never an open proxy |
| Private CA | Certificates for the `.fips` names, CA installed on every participating device | Public CAs cannot issue certificates for internal names; issuance, renewal and trust are yours |
| Own domain over the mesh | Certificate via ACME DNS-01 for a domain you control | A static AAAA record to a FIPS address is not enough: FIPS learns the peer's key only from a `.fips` lookup, so the client's resolver must go through it |
| Local Network Access | `fetch(url, { targetAddressSpace: "local" })` from your own web client, after the user grants permission | Chromium only (Chrome 142+); does not help plain image or video URLs |

With native TLS, Almond serves only HTTPS on that listener and loads the
certificate at startup, so renewal needs a restart. Its self-signed certificate
covers only `localhost`, not `.fips` names. Set `ALMOND_PUBLIC_URL` to the
browser-facing HTTPS origin; see [HTTPS](/deployment/https).

**FIPS upstreams.** Set `.fips` hostnames as upstreams; this needs
`FIPS_REWRITE_DNS=true` (the default) so Almond can resolve them:

```bash
-e ALMOND_UPSTREAM_SERVERS=https://npub1upstream....fips
# or with an alias:
-e FIPS_HOSTS='media-cache npub1upstream...'
-e ALMOND_UPSTREAM_SERVERS=https://media-cache.fips
```

With `ALMOND_CUSTOM_ORIGIN_ACCESS=public`, requests may also pass
`?xs=https://media-cache.fips` or `?origin=https://media-cache.fips`. SSRF
protection stays on: private and local addresses remain blocked, while `.fips`
hostnames resolving to FIPS overlay IPv6 addresses are allowed.
