# Almond Config Editor – Specification

> Status: Concept state, verified against `src/config.rs`, `.env.example`, `docker-entrypoint-fips.sh`, README and dotenvy 0.15.7. Replaces the earlier draft, which was written without access to this repo.

## Goal

A purely client-side, technical config editor for Almond. It helps create or edit an Almond `.env` from scratch and re-export it.

The editor is a single, self-contained HTML file. It works via `file://` and is additionally served by the Almond server at `/config`.

## Non-goals

- No database, no user account, no server-side configuration state.
- No automatic application of a configuration to a running Almond server.
- No wizard as a separate product path; the first cut is a technical editor.
- No share link for configurations or secrets.
- No Compose export in the first cut (rationale below).

## Settled product decisions

### Storage and network

- All configuration data stays in the browser.
- The application does not perform any network communication after loading: no API calls, telemetry, analytics, external scripts, CDN assets, fonts, or images.
- Configurations are not stored in `localStorage` or IndexedDB.
- The variant served by the Almond server requires the initial HTTP request; after that, no further requests are permitted.

### Version scope

The editor only needs to work with the current Almond version at any given time. No multi-version schema, no configuration migration.

### Variable scope

First cut: **only the variables from `Config::from_map`** (`src/config.rs`). That is a closed, testable schema with real invariants present in the code.

Deliberately out of scope:

- `RUST_LOG` — not read by `config.rs`, but set in the `Dockerfile` and interpreted by `tracing`.
- The roughly 18 `FIPS_*` variables from `docker-entrypoint-fips.sh` (`FIPS_NSEC`, `FIPS_PEERS`, `FIPS_HOSTS`, `FIPS_PEER_*`, `FIPS_TUN_*`, `FIPS_REWRITE_DNS`, `FIPS_ISOLATE`, …). They are a second schema in a shell file, and two of them are multi-line.
- `TLS_CERT` / `TLS_KEY` (inline PEM), also only in the FIPS entrypoint.

These variables are treated like all other unknown variables on import: preserved, exported unchanged, marked as "not part of the Almond core schema".

### Origin of the field schema

Almond has roughly 50 variables with defaults, types, and cross-field invariants in `src/config.rs` alone. Help texts, categories, sensitivity, and display order, however, do not exist there at all.

**The schema is a static table in the editor.** It is not generated from `config.rs`, and `config.rs` is not touched for this project. The table lives directly in the HTML file's inline script as a `const ALMOND_SCHEMA = [ ... ];` literal between marker comments (see artifact section).

Per variable the table carries:

| Field | Purpose |
| --- | --- |
| `name` | Variable name |
| `kind` | Type family (bool, u64, mebibytes, string, path, list, enum, secret) |
| `default` | Default as a string, or `null` when "not set" is the default |
| `empty` | Behavior for `KEY=`: `like_unset`, `empty_value` or `hard_error` |
| `probe` | Example value that differs from the default — serves as a UI placeholder and as a test probe |
| `category`, `help`, `sensitive`, `risk` | pure editor metadata |
| `values` | Allowed values for enum fields |

### Drift protection

A static table can become outdated when `config.rs` changes. This can be caught almost entirely without touching production code — the tests cut out the `ALMOND_SCHEMA` literal between its markers from the HTML file, parse it as JSON, and call `Config::from_map`. `Config` derives `PartialEq` (`config.rs:179`), so none of the tests need a mapping from table entry to struct field:

1. **Default correctness**: explicitly setting the asserted default must produce the same result as omitting it — `from_map({name: default}) == from_map({})`.
2. **Effectiveness**: the probe value must change the configuration — `from_map({name: probe}) != from_map({})`. Catches typos and dead names in the table, and is the safety net for the four fields that silently fall back to a default on invalid input.
3. **Empty-value family**: `from_map({name: ""})` behaves as asserted under `empty` — same as unset, different, or error. This verifies the table of the three empty-value families below instead of merely asserting it.
4. **Completeness**: the name set of the table must match the set of string literals in `Config::from_map`. This is text analysis, but robust enough here — all names appear as literals in the code. For defaults and special cases, text analysis would be inadequate; tests 1 through 3 cover those behaviorally.

These tests run in Almond's test suite. If one fails, the table is outdated.

If it later turns out that test 4 is too coarse, there is an expansion path: a thin newtype around the `HashMap` in `from_map` that logs queried names would prove completeness rather than estimate it. That is the only point at which this project would ever need to touch production code, and it is deliberately deferred.

### Entry points

Two workflows in the same surface:

1. **New configuration**: starts with exactly the values Almond uses when starting without a `.env`.
2. **Import existing `.env`**: select a file or paste content, edit, re-export.

### Defaults and risk display

The preset corresponds to **Almond's actual runtime defaults**. The editor is an honest reflection of the runtime and does not secretly prefill more strictly.

Almond is open by default: `FEATURE_UPLOAD_ENABLED=public`, `FEATURE_MIRROR_ENABLED=public`, `FEATURE_LIST_ENABLED=true`, `FEATURE_HOMEPAGE_ENABLED=true`. `BIND_ADDR` on the other hand is restrictive with `127.0.0.1:3000`, and `PUBLIC_URL` has a default (`http://127.0.0.1:3000`, or `https://…` when `ENABLE_HTTPS=true`) that works but is wrong for every non-local installation.

The editor visibly marks such settings as a risk or a necessary decision, but does not change them silently. Candidates for this marking:

| Topic | Note |
| --- | --- |
| `BIND_ADDR` not on localhost | Server is reachable from outside |
| `PUBLIC_URL` set to `127.0.0.1` with non-local binding | Generated blob descriptors point nowhere |
| `FEATURE_UPLOAD_ENABLED` / `FEATURE_MIRROR_ENABLED` set to `public` without `ALLOWED_NPUBS` | Open write access |
| `CORS_ALLOWED_ORIGINS` set | Cross-origin read access to discovery and metrics |
| `METRICS_BEARER_TOKEN` set with public binding | `/metrics` reachable |
| `MAX_TOTAL_SIZE` / `MAX_TOTAL_FILES` at default | Practically unlimited (99999 MiB / 99999999 files) |
| `MAX_FILE_AGE_DAYS=0` | No age expiry for uploads |
| `ENABLE_HTTPS=true` with `TLS_AUTO_GENERATE=true` | Self-signed, development only |

### Export

- Canonical and only export in the first cut is a `.env` file.
- **No Compose export.** The repo contains no canonical non-FIPS `docker-compose.yml`. The only existing Compose file (`docker-compose.fips.yml`) is a FIPS test configuration with concrete peer addresses and `build:` instead of `image:`, and it deviates from the Compose snippet in the README. A Compose export would therefore have to invent a manifest that does not exist in the project. Once a canonical `compose.yaml` exists in the repo, the export can be retrofitted — it would then reference the `.env` via `env_file` and contain no secrets itself.
- Coolify therefore also does not need a separate export; the README currently describes only the FIPS path for Coolify, which is outside this cut.

### Secrets

Secrets in the core schema: `ALMOND_S3_ACCESS_KEY_ID`, `ALMOND_S3_SECRET_ACCESS_KEY`, `METRICS_BEARER_TOKEN`, `P2P_NSEC`.

- Secrets are masked in the UI and only visible on explicit action.
- Secrets must never end up in URLs, logs, telemetry, or error messages.
- The UI warns on export that the `.env` may contain secrets and should not be checked in.
- **Only `METRICS_BEARER_TOKEN` is generated** (and future free-text tokens), exclusively via `crypto.getRandomValues()`.
- **`P2P_NSEC` is not generated.** It is a secp256k1 key in bech32 format; `getRandomValues` alone is not sufficient for that, and secp256k1 plus bech32 code in a file that users open from arbitrary sources is not worth the attack surface. The editor only validates the prefix and length and refers to external tools.
- Docker secrets and platform secret stores are not part of the first cut.

## Import and document model

The `.env` import cannot be reduced to a `Record<string, string>`. That would lose unknown variables, comments, order, ambiguous entries, and empty values.

Two levels:

```text
.env document
  ├─ blank lines
  ├─ comments
  ├─ known assignments
  ├─ unknown assignments
  └─ incomprehensible or unsupported lines

Effective Almond configuration
  └─ known variables, validated against the current Almond schema
```

### States of a variable

A `.env` has three states, and the export must preserve all three:

1. **Not set**: variable is not part of the `.env`; Almond's runtime default applies.
2. **Set**: variable has a concrete value.
3. **Explicitly empty**: variable is present but empty (`KEY=`).

**The UI nevertheless models this not as three states, but as two inputs per variable: a checkbox "is written" and a value box.** Checked and value box empty *is* the case `KEY=`; a third, separately toggleable state would only be a second truth about the same empty box and would need to be kept permanently in sync with it.

What that means in use:

- Default is everything unchecked: a fresh configuration writes no lines at all.
- An unchecked field is **not** `disabled`, but only shows the applicable default as a placeholder. Clicking into it checks the variable — hitting the checkbox is never necessary to set something. Keyboard traversal (Tab) on the other hand does not check anything, otherwise tabbing through all fields would enable fifty variables.
- Unchecking removes the line again, including an imported one.

"Not set" is not identical to `false` or an empty string — and "explicitly empty" means something completely different in Almond depending on the field. The code has three families, and the schema table must carry that distinction in its `empty` field:

| Family | Behavior for `KEY=` | Examples |
| --- | --- | --- |
| `parse_opt`, `parse_list` | like "not set" | `METRICS_BEARER_TOKEN`, `P2P_NSEC`, `UPSTREAM_SERVERS`, `CORS_ALLOWED_ORIGINS`, all `ALMOND_S3_*` |
| `parse_str`, `parse_path` | empty string or empty path, **not** the default | `STORAGE_PATH`, `PUBLIC_URL`, `SERVE_FILES_MANIFEST_NAME`, `TLS_CERT_PATH` |
| `parse_into`, `parse_u64`, `parse_usize`, `parse_bool`, `parse_mebibytes` | **hard startup error** | `MAX_TOTAL_SIZE`, `CLEANUP_INTERVAL_SECS`, `ENABLE_HTTPS`, `AUTH_MAX_TTL_SECS`, … |

The editor must display "checked but empty" as an error for the third family, not as a neutral state, and explain for the first family that Almond reads `KEY=` as "not set" there.

### Fields that swallow bad values

Four fields accept any value and silently fall back to a default on invalid input instead of aborting startup:

- `FEATURE_UPLOAD_ENABLED`, `FEATURE_MIRROR_ENABLED`, `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED`, `FEATURE_REPORT_ENABLED` → `FeatureMode::from_str_with_default` (`src/models.rs:32`)
- `UPSTREAM_MODE` → falls back to `proxy` (`src/models.rs:122`)
- `REPORT_ACTION` → everything except `delete` becomes `quarantine` (`src/models.rs:170`)
- `FILTER_ALGORITHM` → falls back to `binary-fuse-16` (`src/config.rs:159`)

This is security-relevant: `FEATURE_UPLOAD_ENABLED=disabled` is a typo that Almond interprets as `public` — so open instead of closed. Likewise, `ALLOWED_NPUBS` silently discards invalid npubs (`src/config.rs:455`, deliberately commented as a footgun in the code): a typo silently shrinks the whitelist.

**The editor must strictly validate these values**, even though Almond lets them through. This is one of the strongest reasons for the editor's existence.

### Unknown variables

- Marked on import as "not understood by the current editor".
- Exported unchanged and in original form. This includes `FIPS_*`, `RUST_LOG`, and everything else outside the core schema.
- Comments and blank lines are preserved.
- Newly set known Almond variables are added in a clearly demarcated Almond configuration block.

### Duplicate keys

**The first assignment wins, not the last.** dotenvy only sets a variable if it does not already exist in the environment (`iter.rs:34`, `if env::var(&key).is_err()`); every subsequent assignment of the same key is silently discarded.

This is exactly the opposite of the common expectation and also opposite to Docker Compose's own env parser. The editor must:

- determine the effective value by dotenvy semantics (first assignment),
- visibly warn about duplicates and mention that Docker Compose resolves them differently,
- not silently remove duplicates in the default export.

An explicit "clean up duplicate variables" function can be added later, but not as default behavior.

### Process environment beats `.env`

dotenvy does not overwrite an already set environment variable. In a container, `environment:` therefore always beats a mounted `.env`. The editor cannot know the process environment; the export view should name this caveat once, rather than promise an effective value it cannot guarantee.

### Syntax scope

What dotenvy 0.15.7 actually supports and what the editor must therefore correctly read and output:

```dotenv
KEY=value
KEY=
KEY="quoted value"
KEY='literal value'
# Comment
KEY=value # End-of-line comment
export KEY=value
KEY="multi-line
value"
```

Rules, all verified against the parser:

- **Multi-line values are supported**, across open quotes (`iter.rs`, states `StrongOpen`/`WeakOpen`). They are not an "unsupported" case and must be preserved as such values.
- **`${VAR}` and `$VAR` are resolved** — from the process environment or from variables defined earlier in the same file (`parse.rs:260`). No substitution occurs inside `'single quotes'`.
- End-of-line comments need whitespace: `KEY=v #k` is a comment, `KEY=v#k` makes `v#k` the value.
- `KEY=v w` (unquoted, with spaces) is a **parse error**.
- Inside `"…"` the escapes `\\`, `\'`, `\"`, `\$`, `\ ` and `\n` apply; any other `\x` is an error.
- Inside `'…'` there is no escaping — an apostrophe cannot be represented there.
- Whitespace around names and `=` is tolerated on import.

**Critical for export quality:** `Config::from_env` calls `let _ = dotenvy::dotenv();` (`src/config.rs:542`) — parse errors are discarded. A single broken line aborts the iterator, all subsequent variables are ignored, and Almond starts with defaults without any message. An export that somewhere quotes carelessly is therefore not a visible error, but a silent misconfiguration.

Hence the export rule: **every value written by the editor is conservatively quoted.** Values containing `$`, `#`, whitespace, quotation marks, or line breaks must never be output unquoted. If a value does not contain `'`, `'…'` is the safest form because it rules out substitution. Otherwise `"…"` with full escaping. A round-trip test against a dotenvy-compatible parser belongs in the test suite.

### Handling `${VAR}` references

Imported values with variable references are **preserved unchanged**. The field is marked as "not literal", with the note that Almond substitutes at runtime and the displayed raw value is therefore not the effective value. The editor does not resolve and does not validate the resolved value — it does not know the target server's process environment.

### Validation

- Known variables are validated against the static schema table: type, required status, behavior on empty value, default, sensitivity, help text, dependencies, and bounds.
- The cross-field invariants present in the code must be replicated:
  - `ALMOND_S3_*`: all four or none (`config.rs:323`).
  - `MAX_CHUNK_SIZE_MB ≤ MAX_BLOB_SIZE_MB` (`config.rs:356`).
  - Exactly one `CASHU_ACCEPTED_MINTS` once a paid feature is enabled (`config.rs:417`).
  - `DVM_ALLOWED_KINDS` not empty when Upload or Mirror runs in `dvm` mode (`config.rs:435`).
  - `CLEANUP_INTERVAL_SECS > 0` (`config.rs:345`).
  - `BIND_ADDR` must parse as a `SocketAddr` (`config.rs:287`).
  - `ENABLE_HTTPS` determines the default of `PUBLIC_URL` (`config.rs:298`).
- Additionally, the editor strictly validates the enum fields that Almond silently lets through (see above), and checks npubs in `ALLOWED_NPUBS` client-side.
- Unknown variables are not a validation error.
- Invalid known values are displayed per field and per line.
- The export is blocked while known Almond settings are invalid. Incomprehensible raw lines do not block.

> Note about the repo: `.env.example` claims for the paid features "accept only `on`; any other value disables the feature". That is no longer true — `parse_bool` accepts `true/false/1/0/on/off` and throws a startup error on anything else. The comment should be corrected independently of this project.

## User interface

Targeted at technically proficient users.

- File import and paste of `.env` content.
- Categories, search, and short context-specific help texts.
- Per variable exactly one checkbox "is written to the `.env`" plus a value box; unwritten variables show the applicable default as a placeholder.
- Live validation, warnings, and risk markings.
- Masked secret fields; generator only for free-text tokens.
- Raw view or `.env` preview as an export view with copy function.
- Download of the `.env`.

The raw view is not a second freely editable source. The form state and the lossless document model remain authoritative.

## Delivery and security

### Artifact: one file, no build

The editor is **a hand-written HTML file with no build step**. There is no bundler, no npm, no framework, and no generation run. The file in the repo is byte-identical to the one that runs in the browser and that the server serves.

The repo already has a precedent for this: `src/filter-test.html` is 466 lines of hand-written HTML with an inline script, served via `include_str!` under its own route (`src/main.rs:135`).

That dictates the structure:

- **A single inline `<script>`.** No `import` — under `file://` every module import fails due to CORS. The logic is vanilla JS against the DOM: a form over roughly 50 fields, no SPA.
- **Inline `<style>`**, no external CSS.
- **The schema table lives in the same script**, as a `const ALMOND_SCHEMA = [ ... ];` literal between two marker comments (`/* ALMOND_SCHEMA_JSON_START */` … `/* ALMOND_SCHEMA_JSON_END */`). A separate `<script type="application/json">` would be the more obvious separation, but `script-src` according to the CSP specification covers every `<script>` element regardless of its `type` — such a block would therefore not be reliably exempt from the hash. The marker comments still give the drift tests a trivial extraction point: substring between the markers, strip `const ALMOND_SCHEMA = ` and the trailing `;`, parse as JSON.
- No dynamic imports, no service or web workers, no external files.

Realistic size: the schema table for 50 fields with help texts, plus parser, exporter, validation, and rendering — roughly 2500 to 3500 lines in one file. Large, but navigable and readable without a toolchain.

### Delivery by Almond

The server serves the file at `/config`, analogous to the homepage via `include_str!` (`src/main.rs:125`). The route is tied to `FEATURE_HOMEPAGE_ENABLED` — no new flag; whoever disables the homepage also disables the editor.

### Verification instead of generation

Without a build there is no step that could generate anything. The guiding principle is therefore consistent throughout: **nothing is generated, everything is verified.** All checks run in Almond's regular `cargo test` suite against the checked-in HTML file:

- The four drift tests against `Config::from_map` (see above), which read the schema table from the `ALMOND_SCHEMA` literal.
- No `fetch`, `XMLHttpRequest`, `WebSocket`, `EventSource`, or `sendBeacon` in the artifact.
- No external URLs in `src`, `href` or `url(...)`.
- The CSP hash in the meta tag matches the current inline script (see below).
- **Export fixtures**: a handful of checked-in `.env` files with deliberately nasty values (`$`, `#`, apostrophe, quotation marks, spaces, line breaks) alongside their expected interpretation. The Rust test parses them with dotenvy and checks that the expected values come out. The fixtures are the shared artifact between JS and Rust and the actual proof that the quoting rules are correct.

### Self-test in the browser

The JS logic itself — dotenv parser, exporter, round trip, validation — is tested via a self-test page built into the editor, reachable at `#selftest`. It runs the test cases in the browser and displays the result; the same export fixtures serve as expected values. This too follows the pattern of `filter-test.html`.

Without a build there is no Node test runner, and introducing one would undo the decision against a toolchain.

### CSP

The CSP is set **in the file itself** as a `<meta http-equiv="Content-Security-Policy">`, so that it also takes effect under `file://` and when sharing the file. Target policy:

```http
default-src 'none';
script-src 'sha256-<hash of the inline script>';
style-src 'sha256-<hash of the inline styles>';
img-src data:;
connect-src 'none';
font-src 'none';
media-src 'none';
object-src 'none';
worker-src 'none';
form-action 'none';
base-uri 'none'
```

Two points to consider during implementation:

- **`frame-ancestors` does not work in a meta tag** and is ignored by the browser (likewise `sandbox` and `report-uri`). If clickjacking protection is desired, Almond must set `frame-ancestors` or `X-Frame-Options` as an HTTP header on the `/config` route. Almond currently sets no security headers at all — that would be a new, small addition.
- **The download path must be tested under this CSP.** A download via a blob or data URL can fail under `default-src 'none'` depending on the browser. If that is the case, the copy-to-clipboard function is the reliable path and the download is the convenience path, not the other way around.

The price of build freedom lies exactly here: **the two hashes are maintained by hand.** Anyone changing the inline script must update the hash in the meta tag. This is safeguarded by a test in `cargo test` that computes the hash over the actual script content and compares it with the meta tag; on mismatch it outputs the correct value to insert. A forgotten hash is therefore a red test and not a silent failure — but it remains a manual copy-paste step per script change.

Since the schema and code live in the same script, every schema edit changes the hash as well — that is the price of CSP security under the meta-tag model, not just of code changes.

The primary security guarantee remains the absence of network-capable code paths; the CSP is the second line of defense.

## Open items

1. Does the export preserve the imported order even for known variables, or may known fields be rearranged into a canonical order? Recommendation: preserve imported structure, only add newly set values deliberately.
2. Explicit duplicate cleanup as a later function. Recommendation: yes, but not as default behavior.
3. Optional, explicitly enabled local draft storage. Recommendation: not in the first cut; no secrets in it.
4. Compose export, once a canonical `compose.yaml` exists in the repo.
