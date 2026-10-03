# Agents

Project overview, build commands and conventions: `GEMINI.md`. Domain glossary: `CONTEXT.md`.

Commit messages, PR titles, issue comments and code comments are in English: this is a public repository.

Before opening a PR, run what CI (`.github/workflows/rust.yml`) runs: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build`, `cargo test`.

## Agent skills

### Issue tracker

Issues are tracked as GitHub issues in `flox1an/almond`, via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Triage uses the five canonical labels without overrides. See `docs/agents/triage-labels.md`.

### Domain docs

Domain documentation uses a single-context layout. See `docs/agents/domain.md`.
