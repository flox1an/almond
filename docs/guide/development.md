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
