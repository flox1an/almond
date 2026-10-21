# HTTPS

Behind a TLS-terminating reverse proxy (Caddy, nginx, Traefik), leave
`ALMOND_TLS_ENABLED=false`. Set `ALMOND_PUBLIC_URL` to the public `https://`
origin.

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
