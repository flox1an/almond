# Rust library

Almond is also a library crate. It mounts the Blossom routes in your own
[Axum](https://github.com/tokio-rs/axum) 0.8 application. The standalone
`almond` binary is a thin host around the same library and keeps its CLI,
defaults, routes, TLS and signal handling.

## Add the dependency

Almond is not published on crates.io. Depend on a release tag:

```toml
[dependencies]
almond = { git = "https://github.com/flox1an/almond", tag = "v0.5.1", default-features = false }
axum = "0.8"
tokio = { version = "1", features = ["full"] }
```

| Feature | Default | Effect |
|---|---|---|
| `cashu` | on | Cashu payments ([BUD-07](/blossom/#support-matrix)). Without it, configuring paid operations fails startup; it never silently makes them free. |

Omit `default-features = false` to include Cashu payments, as the standalone
binary does.

## Minimal host

This is [`examples/embedded.rs`](https://github.com/flox1an/almond/blob/main/examples/embedded.rs),
a runnable host with its own homepage, a health route and graceful shutdown:

<<< @/../examples/embedded.rs

Run it from a checkout. It listens on `127.0.0.1:3000` and stores blobs in
`./embedded-files`; pass another address after `--`:

```bash
cargo run --example embedded --no-default-features
cargo run --example embedded --no-default-features -- 127.0.0.1:3001
```

## API

| Item | Purpose |
|---|---|
| `Config::defaults()` | Built-in defaults. Reads neither argv nor the environment. |
| `Config::validate(self)` | Cross-field checks, plus normalization of a blank metrics token. `build_state` runs the same checks. |
| `build_state(&cfg).await` | Validates the config, then initializes storage, indexes, HTTP clients and metrics. Returns `AppState`. |
| `create_app(state)` | The Blossom routes (`/upload`, `/mirror`, `/list`, `/report`, `/{filename}`) as a `Router`. |
| `standalone_extras(state)` | Optional: Almond's homepage, favicon, config editor, `/filter`, `/metrics` and diagnostics. |
| `spawn_background_tasks(&state, &cfg)` | Starts the periodic cleanup and refresh jobs and returns them as a `JoinSet<()>`. |
| `AppState::s3_status()` | `None` without S3. Otherwise `Available`, or `Unavailable { error, last_attempt }`. |

The fields of `Config` are the [command-line flags](/configuration/cli) in
`snake_case` (`--upload-access` → `cfg.upload_access`); see the
[environment reference](/configuration/environment#reference) for every setting.

```rust
use almond::{models::FeatureMode, Config};

let mut cfg = Config::defaults();
cfg.storage_path = "/var/lib/my-app/blobs".into();
cfg.public_url = Some("https://media.example.com".into());
cfg.upload_access = FeatureMode::Off;
cfg.upstream_servers = vec!["https://blossom.primal.net".into()];
```

## Host responsibilities

- **Configuration.** The embedding path reads no process arguments, no
  environment variables and no dotenv files. `Config::load()` and
  `config::load_env_file()` are helpers for the standalone binary. Do not call
  them inside a host that is already running.
- **Runtime.** The host owns its runtime, tracing, allocator, TLS listener and
  shutdown policy.
- **rustls provider.** Install a rustls `CryptoProvider` before using WoT or
  DVM access, or custom-origin discovery: `?as=` can query Nostr relays.
  Without a provider, `build_state` returns an error for those settings. For
  example, with `rustls = { version = "0.23.46", features = ["aws-lc-rs"] }`,
  call `rustls::crypto::aws_lc_rs::default_provider().install_default()` at
  startup. Almond never replaces an installed provider. Local or public mode
  with custom-origin discovery disabled needs no global provider.
- **Routing.** Merge `create_app(state)` into the host router. `/{filename}`
  is a catch-all at the root, so the host's explicit routes (`/`, `/health`)
  still take precedence.
- **Background jobs.** Keep the `JoinSet` from `spawn_background_tasks` alive
  while serving, and build it from the same config as the state. Dropping the
  set aborts the jobs. Request-triggered upstream and HLS jobs manage their own
  lifecycle.
- **One state per storage root.** Set `public_url` to the externally reachable
  Blossom origin. Build one state for each storage root and clone it for the
  routers: startup clears temporary files, then migrates and indexes the
  existing blobs.
