# FIPS mesh image

`ghcr.io/flox1an/almond-fips` runs FIPS, dnsmasq and Almond in one container. It
accepts every Almond variable and can, at the same time:

- serve Almond over normal HTTP(S) via Docker port publishing,
- serve the same instance to peered FIPS nodes at `http://<this-node-npub>.fips:3000`,
- fetch from upstream Blossom servers reachable only inside the mesh.

**Requirements.** Unlike the standard image, the FIPS image runs as root and
needs `--cap-add NET_ADMIN` and the host TUN device
(`--device /dev/net/tun:/dev/net/tun`): the entrypoint creates the virtual IPv6
interface (usually `fips0`) and installs small DNS and iptables rules. If
`/dev/net/tun` is missing on a Linux host, run `sudo modprobe tun`.

**FIPS variables:**

| Setting | Description |
|---|---|
| `FIPS_NSEC` | Node secret key, hex or `nsec1`; required unless you mount a full config and set `FIPS_GENERATE_CONFIG=false` |
| `FIPS_PEER_NPUB`, `FIPS_PEER_ADDR`, `FIPS_PEER_ALIAS`, `FIPS_PEER_TRANSPORT` | Optional single direct peer |
| `FIPS_PEERS` | Optional multi-peer list, one `npub,addr[,alias[,transport]]` per line; overrides the single-peer variables |
| `FIPS_UDP_BIND` | UDP transport bind address (default `0.0.0.0:2121`) |
| `FIPS_TUN_NAME`, `FIPS_TUN_MTU` | TUN interface settings |
| `FIPS_REWRITE_DNS` | `true` (default): dnsmasq sends `.fips` names to FIPS and everything else to Docker's DNS. `false`: FIPS still hosts Almond on `fips0`, but container-wide `.fips` DNS is not installed |
| `FIPS_HOSTS` | Optional newline-separated aliases for `/etc/fips/hosts`, e.g. `my-upstream npub1...` makes `https://my-upstream.fips` resolve |
| `FIPS_ISOLATE` | `true` blocks non-FIPS egress on the physical container interface (mesh-only) |

**Run it.** Bind Almond to all interfaces so both the published port and the mesh
reach it:

```bash
docker run \
  --cap-add NET_ADMIN \
  --device /dev/net/tun:/dev/net/tun \
  --sysctl net.ipv6.conf.all.disable_ipv6=0 \
  -p 3000:3000 \
  -p 2121:2121/udp \
  -v /path/to/files:/app/files \
  -e ALMOND_STORAGE_PATH=/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:3000 \
  -e ALMOND_PUBLIC_URL=https://public.example.com \
  -e FIPS_NSEC=nsec1... \
  -e FIPS_PEER_NPUB=npub1gateway... \
  -e FIPS_PEER_ADDR=203.0.113.10:2121 \
  ghcr.io/flox1an/almond-fips:main
```

Public clients use `https://public.example.com`; FIPS peers use
`http://<this-node-npub>.fips:3000`. For mesh-only operation, drop the `3000:3000`
mapping and keep the transport port `2121/udp`.

**HTTPS inside the mesh** uses Almond's normal TLS settings; peers then use
`https://<this-node-npub>.fips:3000`. The certificate must be valid for the
hostname clients use, or clients must trust it explicitly.

```bash
-e ALMOND_TLS_ENABLED=true \
-e ALMOND_TLS_CERT=/app/certs/cert.pem \
-e ALMOND_TLS_KEY=/app/certs/key.pem \
-v /path/to/certs:/app/certs
```

**Short names.** `-e FIPS_HOSTS='my-almond npub1thisnode...'` lets peers with the
same mapping use `http://my-almond.fips:3000`. The canonical `<npub>.fips` name
works without aliases.

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

### Docker Compose

For normal operation, use the bundled
[`docker-compose.fips.yml`](https://github.com/flox1an/almond/blob/main/docker-compose.fips.yml). It carries the settings
FIPS always needs (`NET_ADMIN`, `/dev/net/tun`, IPv6 sysctl, ports, storage
volume, DNS defaults), so per-node configuration stays small. Copy
[`.env.fips.example`](https://github.com/flox1an/almond/blob/main/.env.fips.example) to `.env.fips` and fill in your node:

```dotenv
ALMOND_PUBLIC_URL=http://<this-node-npub>.fips:3000
ALMOND_UPSTREAM_MODE=proxy
ALMOND_UPSTREAM_SERVERS=

FIPS_NSEC=nsec1...
FIPS_PEER_NPUB=
FIPS_PEER_ADDR=
FIPS_PEER_ALIAS=gateway
FIPS_PEER_TRANSPORT=udp
FIPS_PEERS=
FIPS_HOSTS=
```

`FIPS_PEERS` is recommended in production: with several gateways Almond keeps
routing when one is unavailable, which matters for large transfers.

```dotenv
FIPS_PEERS=npub1aaa...,203.0.113.10:2121,gateway-a,udp
npub1bbb...,198.51.100.20:2121,gateway-b,udp
```

```bash
docker compose --env-file .env.fips -f docker-compose.fips.yml up -d
```

The compose service looks like this:

```yaml
services:
  almond-fips:
    image: ghcr.io/flox1an/almond-fips:main
    cap_add:
      - NET_ADMIN
    devices:
      - /dev/net/tun:/dev/net/tun
    sysctls:
      - net.ipv6.conf.all.disable_ipv6=0
    ports:
      - "3000:3000"
      - "2121:2121/udp"
    environment:
      ALMOND_BIND_ADDR: 0.0.0.0:3000
      ALMOND_PUBLIC_URL: http://<this-node-npub>.fips:3000
      ALMOND_STORAGE_PATH: /app/files
      ALMOND_UPLOAD_ACCESS: public
      ALMOND_MIRROR_ACCESS: public
      ALMOND_CUSTOM_ORIGIN_ACCESS: public
      ALMOND_UPSTREAM_MODE: proxy
      FIPS_NSEC: nsec1...
      FIPS_PEER_NPUB: npub1gateway...
      FIPS_PEER_ADDR: 203.0.113.10:2121
      FIPS_PEER_ALIAS: gateway
      FIPS_PEERS: |
        npub1aaa...,203.0.113.10:2121,gateway-a,udp
        npub1bbb...,198.51.100.20:2121,gateway-b,udp
      FIPS_REWRITE_DNS: "true"
    volumes:
      - almond-files:/app/files

volumes:
  almond-files:
```

FIPS upstreams in compose:

```yaml
      ALMOND_UPSTREAM_SERVERS: https://npub1upstream....fips,https://media-cache.fips
      FIPS_HOSTS: |
        media-cache npub1upstream...
```

### Coolify

Use Coolify's Docker Compose deployment mode: the compose file is then the
source of truth, so `cap_add`, `devices`, `sysctls`, ports, volumes and
environment stay together.

1. Check that the target server has `/dev/net/tun` (`ls -l /dev/net/tun`); if not,
   run `sudo modprobe tun` on the Linux host.
2. Create a Docker Compose resource and paste the `docker-compose.fips.yml` service.
3. Set the variables:

   ```dotenv
   ALMOND_PUBLIC_URL=https://your-public-domain.example
   ALMOND_UPSTREAM_MODE=proxy
   ALMOND_UPSTREAM_SERVERS=
   FIPS_NSEC=nsec1...
   FIPS_PEER_NPUB=npub1gateway...
   FIPS_PEER_ADDR=203.0.113.10:2121
   FIPS_PEER_ALIAS=gateway
   FIPS_PEER_TRANSPORT=udp
   FIPS_PEERS=
   FIPS_HOSTS=
   ```

4. For public HTTP(S), assign the Coolify domain to the `almond-fips` service on
   container port `3000`. Keep `2121:2121/udp` published for the FIPS transport.

In image-only mode instead of Compose mode, put the runtime settings into Custom
Docker Options; the UDP port, volume and DNS defaults then have to be configured
separately:

```text
--cap-add NET_ADMIN --device /dev/net/tun:/dev/net/tun --sysctl net.ipv6.conf.all.disable_ipv6=0
```

If your hosting provider rejects `devices`, `cap_add` or `sysctls`, FIPS cannot
run inside the container. Run FIPS on the host instead, or use a VPS or
bare-metal server where Docker can access `/dev/net/tun`.
