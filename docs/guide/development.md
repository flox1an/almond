# Development

Prerequisites: Rust 1.85 or later and the OpenSSL development libraries.

```bash
cargo build --release
cargo run --release
```

Before opening a PR, run what CI runs:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build
cargo test
```

## Documentation

This site is built with [VitePress](https://vitepress.dev) from `docs/`.
Every push to `main` that touches `docs/` deploys it to GitHub Pages
(`.github/workflows/docs.yml`); pull requests only run the build, which fails
on dead links.

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
