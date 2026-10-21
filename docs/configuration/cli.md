# Command-line flags

Every setting is also a flag, so a server needs no config file. `almond --help`
lists every flag with its environment name and default.

**Minimal server**: `127.0.0.1:3000`, blobs in `./files`, uploads open to any
valid Nostr signature:

```bash
almond
```

**Personal server**: only your npubs may upload, reachable from outside:

```bash
almond --bind-addr 0.0.0.0:3000 \
  --public-url https://blossom.example.com \
  --storage-path /var/lib/almond \
  --allowed-npubs npub1you...,npub1friend...
```

**Public temporary host**: anyone uploads, blobs expire after a week, storage capped:

```bash
almond --bind-addr 0.0.0.0:3000 \
  --public-url https://tmp.example.com \
  --upload-max-age 7d \
  --storage-max-size 50GiB \
  --blob-max-size 100MiB
```

**Community server**: uploads for you and your web of trust:

```bash
almond --upload-access wot --allowed-npubs npub1you...
```

**Caching edge**: no uploads, serves and caches blobs from upstream servers:

```bash
almond --upload-access off --mirror-access off \
  --upstream-servers https://cdn.satellite.earth,https://blossom.primal.net
```

**Local Blossom Cache** on the standard port:

```bash
almond --bind-addr 127.0.0.1:24242 --public-url http://127.0.0.1:24242 \
  --upload-access off --mirror-access off \
  --custom-origin-access public \
  --storage-max-size 5GiB --upstream-cache-ttl 30d
```

**HTTPS for local testing** with a generated self-signed certificate:

```bash
almond --tls-enabled --tls-self-signed
```

## CLI syntax

- A bare `--tls-enabled` means true; `--list-enabled=false` disables a
  boolean flag.
- Durations use `<n><unit>` with `s`, `m`, `h`, `d` (`30s`, `5m`, `24h`, `7d`).
- Sizes use `B`, `KiB`, `MiB`, `GiB`, `TiB` (IEC only; `MB`/`GB` are rejected).
- List options accept comma-separated values and repeating the flag.
- Access modes are `off`, `public`, `wot`, `dvm`.

See [Configuration overview](/configuration/) for precedence between flags,
environment variables and the config file.
