# Docker

The image `ghcr.io/flox1an/almond` sets container defaults: bind `0.0.0.0:3000`,
storage `/app/files`, at most 1,000,000 files, cleanup every `60s`, and TLS and
wallet files under `/app/state`. Build it locally with `docker build -t almond .`.

The image runs as the fixed, non-root numeric identity `10001:10001`. This is an
image contract, not a host-user name: `ls` resolves it through the host's
`/etc/passwd` and may show a different name. The image never changes host-volume
ownership.

A Docker-managed named volume needs no preparation:

```bash
docker volume create almond-files
docker run --rm -p 3000:3000 \
  -v almond-files:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

A host bind mount must be owned by the image identity:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

Migrate an existing bind mount once before upgrading to an image with this
contract:

```bash
sudo chown -R 10001:10001 /data/almond
sudo find /data/almond -type d -exec chmod 0750 {} +
sudo find /data/almond -type f -exec chmod 0640 {} +
```

To run as a different user (for example your own account), override it; the
chosen pair must own the host directory:

```yaml
services:
  almond:
    image: ghcr.io/flox1an/almond:v0.5.1
    user: "${PUID:-10001}:${PGID:-10001}"
    ports:
      - "3000:3000"
    volumes:
      - /data/almond:/app/files
    environment:
      ALMOND_STORAGE_PATH: /app/files
```

```bash
PUID=$(id -u) PGID=$(id -g) docker compose up -d
```

For TLS auto-generation or Cashu payments, mount persistent state separately
with the same UID/GID:

```bash
sudo install -d -o 10001 -g 10001 -m 0750 /data/almond-state
docker run --rm -p 3000:3000 \
  -v /data/almond:/app/files \
  -v /data/almond-state:/app/state \
  -e ALMOND_TLS_ENABLED=true \
  ghcr.io/flox1an/almond:v0.5.1
```
