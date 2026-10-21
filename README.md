```
        .__                             .___
 _____  |  |   _____   ____   ____    __| _/
 \__  \ |  |  /     \ /  _ \ /    \  / __ | 
  / __ \|  |_|  Y Y  (  <_> )   |  \/ /_/ | 
 (____  /____/__|_|  /\____/|___|  /\____ | 
      \/           \/            \/      \/  
```

**Almond** (Any Large Media ON Demand) is a [Blossom](https://github.com/hzrd149/blossom)
media server for Nostr. It stores blobs on the local filesystem or in an
S3-compatible bucket, expires them automatically, and can cache blobs from other
Blossom servers. Writes are authorized with Nostr signatures: open to everyone,
limited to an npub allowlist, to a web of trust, or to DVMs.

## Quick start

With Docker:

```bash
docker volume create almond-files
docker run --rm -p 3000:3000 \
  -v almond-files:/app/files \
  ghcr.io/flox1an/almond:v0.5.1
```

From source:

```bash
cp .env.example .env    # optional, edit as needed
cargo run --release     # listens on 127.0.0.1:3000
```

Almond is now at `http://localhost:3000`. Out of the box anyone with a valid
Nostr signature may upload and mirror, and nothing expires. Before exposing it,
set at least `ALMOND_PUBLIC_URL` and either `ALMOND_ALLOWED_NPUBS` or a retention
limit:

```dotenv
ALMOND_PUBLIC_URL=https://blossom.example.com
ALMOND_ALLOWED_NPUBS=npub1...
```

## Documentation

Full documentation: **<https://flox1an.github.io/almond/>**, built with
[VitePress](https://vitepress.dev) from [`docs/`](docs/):

| Topic | Page |
|---|---|
| Features, deployment variants | [Getting started](docs/guide/getting-started.md) |
| Storage, retention, access control, upstreams, S3 | [How it works](docs/guide/how-it-works.md) |
| Precedence and config file | [Configuration](docs/configuration/index.md) |
| CLI examples | [Command-line flags](docs/configuration/cli.md) |
| Every setting, pre-0.5 migration | [Environment variables](docs/configuration/environment.md) |
| systemd, Docker, HTTPS, S3, Local Blossom Cache, FIPS | [Deployment](docs/deployment/) |
| Embedding in an Axum app | [Rust library](docs/library.md) |
| BUD support and deviations, Almond extensions | [Blossom specs](docs/blossom/index.md) |
| Endpoints, caching headers | [HTTP API](docs/reference/api.md) |

Preview locally with `cd docs && npm install && npm run docs:dev`.

## Development

Prerequisites: Rust 1.85 or later and the OpenSSL development libraries.
Before opening a PR, run what CI runs: `cargo fmt --check`,
`cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`.

## License

MIT
