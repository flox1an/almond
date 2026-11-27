# Development

Prerequisites: Rust 1.95 or later and the OpenSSL development libraries.
`rust-toolchain.toml` pins Rust 1.99.1 for development and CI; both Docker
builders use the same stable release. The library's dependency graph requires
at least Rust 1.94.1; development dependencies require Rust 1.95.

Cargo dependencies are refreshed in `Cargo.lock`. `rusqlite` remains on 0.31
because CDK's SQLite backend requires that series; Cargo permits only one
native `sqlite3` link dependency. Updating it independently is not supported.

```bash
cargo build --release
cargo run --release
```

Before opening a PR, run what CI runs:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo build
cargo test
cargo test --no-default-features
```

CI additionally runs these checks; each can be run locally:

| Check | Command | What it guards |
|---|---|---|
| Dependencies | `cargo deny check` | Advisories, licenses, registry sources (`deny.toml`) |
| MSRV | `rustup run <rust-version> cargo check --lib --bins --locked` | `rust-version` in `Cargo.toml`; dev-dependencies may need a newer rustc |
| Secrets | `gitleaks git --log-opts="--all" .` | Credentials in history; known false positives live in `.gitleaksignore` |
| Workflows | `actionlint` | Syntax and shell errors in `.github/workflows/` |
| Coverage | `cargo llvm-cov report --summary-only` | Report only, no threshold |

`deny.toml` ignores two advisories on purpose. Each entry states a reason and
the condition for removing it; do not add one without both.

A non-blocking CI step reports `unwrap()`/`expect()` in shipped code
(`cargo clippy --lib --bins -- -W clippy::unwrap_used -W clippy::expect_used`).
New request-path uses need a reason; test code is exempt (`clippy.toml`).

## Documentation

This site is built with [VitePress](https://vitepress.dev) from `docs/`.
Every push to `main` that touches `docs/` deploys it to GitHub Pages
(`.github/workflows/docs.yml`); pull requests only run the build, which fails
on dead links.

The [admin interface concept](../admin-concept.md) describes proposed scope,
authentication, configuration ownership and implementation acceptance criteria.
It is a planning document, not a description of currently available features.

Preview locally:

```bash
cd docs
npm install
npm run docs:dev      # live preview
npm run docs:build    # static site in docs/.vitepress/dist
```

## Logo assets

- [Original PNG](/logo-original.png): the supplied artwork, unchanged.
- [Web PNG](/logo.png): the same artwork with a transparent exterior, used in the
  navigation and on the documentation homepage.
- [White almond SVG](/logo-white.svg): a vector outline with a transparent
  background; use it on dark or colored surfaces.
- `favicon.ico`: the gradient logo at 16, 32 and 48 pixels.

These files live in `docs/public/`. The standalone server embeds the white
almond directly in its self-contained homepage and serves the same favicon at
`/favicon.ico`. Disabling the homepage also disables its favicon.
