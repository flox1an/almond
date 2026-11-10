# HTTPS

Behind a TLS-terminating reverse proxy (Caddy, nginx, Traefik), leave
`ALMOND_TLS_ENABLED=false`. Set `ALMOND_PUBLIC_URL` to the public `https://`
origin.

## HTTP protocols and concurrent downloads

Almond's TCP listeners support HTTP/1.1 and HTTP/2; the built-in TLS listener
advertises `h2` and `http/1.1` through ALPN. There is no native HTTP/3 listener.
For HTTP/3, terminate QUIC at an HTTP/3-capable reverse proxy such as Caddy,
expose both TCP and UDP port 443, and keep Almond on the private backend.
HTTP/3 can reduce connection setup latency and cross-stream stalls on lossy
networks; it does not guarantee higher bulk-transfer throughput.

The router's concurrency limit is 256 handler futures per route and method,
not 256 active downloads. A download releases its permit when the response is
returned, before its body finishes streaming. The 60-second handler timeout
does not bound that body transfer or time waiting for a concurrency permit.
Use proxy connection limits and slow-client timeouts to bound resource usage;
do not simply increase the handler limit to admit more downloads.

Local HTTP/1.1 downloads hold a client socket and an open file each. Set the
service's file-descriptor limit for the expected connection count, with room
for upstream sockets and background work (`LimitNOFILE` in systemd or
`--ulimit nofile=...` with Docker). HTTP/2 shares the client socket across
streams, but each local download still opens its own file.

Capacity measurements should use a release build and a separate load
generator. Compare small blobs, large blobs and range requests at increasing
concurrency, recording completed bytes/second, p95/p99 latency, failures,
RSS and open file descriptors. Test warm and cold filesystem caches, slow
clients, TLS, and the actual upstream/S3 backend separately. A loopback test
cannot establish production capacity or HTTP/3 gains.

Public immutable blobs are suitable for an edge cache. Do not cache paid
downloads or bypass Almond by exposing its storage directories: cache hits
would skip payment checks, and retained copies can outlive deletion or
moderation. Keep the current `private, no-store` policy for in-flight upstream
responses: their bytes are not verified against the hash yet.

### Streaming blobs that are still arriving from upstream

A blob that is not cached yet is streamed from one shared upstream transfer
(see [Upstream lookups](/guide/how-it-works#upstream-lookups)). Two proxy
settings decide whether players see that progress:

- **Do not buffer responses.** A buffering proxy holds bytes Almond already
  sent; with nginx, set `proxy_buffering off;` for blob locations. Caddy
  streams by default.
- **Allow long gaps between bytes.** A request for a far range or the MP4 tail
  (`moov` at the end) gets its `206` headers at once, then no bytes until the
  shared transfer reaches that offset; for a 1 GiB video on a slow upstream
  that can take minutes. Raise idle read timeouts accordingly, for example
  nginx `proxy_read_timeout 600s;` (default `60s`), or such seeks fail while
  Almond is still filling.

These streams are bounded inside Almond as well: at most 8 concurrent upstream
fills and 256 response bodies reading from them; beyond that, Almond answers
`503` rather than opening extra upstream transfers.

## Settings

| Setting | Default | Description |
|---|---|---|
| `ALMOND_TLS_ENABLED` | `false` | Terminate TLS in Almond. When enabled, plain HTTP is no longer accepted. |
| `ALMOND_TLS_CERT` | `./cert.pem` | Certificate chain (PEM) |
| `ALMOND_TLS_KEY` | `./key.pem` | Private key (PEM). It should be readable only by the server process. |
| `ALMOND_TLS_SELF_SIGNED` | `false` | Generate a self-signed pair when the files are missing. Leave it off in production so that missing certificates fail startup. |
| `ALMOND_PUBLIC_URL` | `https://127.0.0.1:3000` with TLS | Public origin used in blob descriptors |

## Your own certificate

For example, with Let's Encrypt via certbot:

```bash
almond --tls-enabled \
  --tls-cert /etc/letsencrypt/live/your-domain.com/fullchain.pem \
  --tls-key /etc/letsencrypt/live/your-domain.com/privkey.pem \
  --public-url https://your-domain.com \
  --bind-addr 0.0.0.0:443
```

Ports below 1024 need root or `CAP_NET_BIND_SERVICE` on Linux. Automate
renewal; Almond loads the certificate at startup, so restart it after each
renewal.

With Docker:

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -v /path/to/certs:/app/certs \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_CERT=/app/certs/cert.pem \
  -e ALMOND_TLS_KEY=/app/certs/key.pem \
  -e ALMOND_PUBLIC_URL=https://your-domain.com \
  ghcr.io/flox1an/almond
```

## Self-signed, for development

With `ALMOND_TLS_SELF_SIGNED=true`, Almond writes `cert.pem` and `key.pem` when
they are missing. The certificate is valid for `localhost`, `127.0.0.1` and
`::1`. Browsers warn until you trust it manually.

```bash
almond --tls-enabled --tls-self-signed
```

```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e ALMOND_TLS_ENABLED=true \
  -e ALMOND_TLS_SELF_SIGNED=true \
  ghcr.io/flox1an/almond
```

In the Docker image, generated certificates are written under `/app/state`. To
keep the same certificate across restarts, mount that directory; see
[Docker](/deployment/docker).

## Testing

```bash
curl -k https://localhost:3000/
openssl s_client -connect localhost:3000 -servername localhost </dev/null 2>/dev/null \
  | openssl x509 -noout -subject -ext subjectAltName -dates
```

## Troubleshooting

| Symptom | Check |
|---|---|
| Startup fails with missing certificate | Check the paths in `ALMOND_TLS_CERT`/`ALMOND_TLS_KEY`, or enable `ALMOND_TLS_SELF_SIGNED` for development |
| Certificate generation fails | Check write permission and free space in the certificate directory, and SELinux/AppArmor policies |
| `Permission denied` on bind | Ports below 1024 need extra privileges |
| Invalid certificate format | Both files must be PEM |
| Clients still get `http://` URLs | Set `ALMOND_PUBLIC_URL` to the `https://` origin |
