# Agents

Almond is a Blossom media server for Nostr (Rust: Axum, Tokio, nostr-sdk). User documentation lives in `docs/` (VitePress site); domain glossary: `CONTEXT.md`.

Commit messages, PR titles, issue comments and code comments are in English: this is a public repository.

Before opening a PR, run what CI (`.github/workflows/rust.yml`) runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`. Docs changes: `cd docs && npm run docs:build` (fails on dead links).

## Code layout

- `src/main.rs`: standalone entry point: config, tracing, allocator, crypto, TLS, signals and listener.
- `src/lib.rs`: library API (`build_state`, `create_app`, `standalone_extras`, `spawn_background_tasks`) and periodic jobs. Runnable embedding: `examples/embedded.rs`.
- `src/config.rs`: every setting is one clap argument; its `ALMOND_*` env name is derived from the flag. `.env.example` and `src/config-editor.html` must track it (`src/config_editor_tests.rs` checks the editor).
- `src/handlers/`: Axum request handlers. `src/services/`: business logic (storage, auth, upload/download). `src/models.rs`: shared data structures and `AppState`. `src/trust_network.rs`: web of trust and DVM lists.
- `src/error.rs`: `AppError` maps directly to HTTP status codes.

## Conventions

- Everything is async on Tokio; stream request and response bodies instead of buffering.
- Upstream and mirror fetches keep SSRF protection: private and local addresses stay blocked.
- Cashu is an optional, default-enabled feature (`cashu`).

## Agent skills

### Issue tracker

Issues are tracked as GitHub issues in `flox1an/almond`, via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Triage uses the five canonical labels without overrides. See `docs/agents/triage-labels.md`.

### Domain docs

Domain documentation uses a single-context layout. See `docs/agents/domain.md`.
