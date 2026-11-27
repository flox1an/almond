# Admin interface concept

Status: proposed architecture and product scope, 2026-11-10. This is a planning document, not documentation of an available admin interface. Routes, option names and file layouts below are proposals unless explicitly described as existing.

## 1. Goal and agreed constraints

Provide an integrated web interface for operating one Almond instance: inspect and remove blobs, process reports, manage access and configuration, understand server health, and administer the optional Cashu wallet.

- Operators and explicitly added collaborators have full administrative access. No moderator role or general permissions framework is planned.
- Use Nostr keys instead of a user database: no registration, passwords, profiles or account recovery service.
- Add an authenticated administrative REST API; do not change Blossom authentication into an admin authentication mechanism.
- Do not add a database for administration or configuration. The optional Cashu feature already uses SQLite; this proposal neither removes it nor makes administration depend on it.
- Preserve CLI, environment and existing config-file deployments. Browser changes must never silently defeat explicit deployment settings.
- Support both externally managed installations and installations that allow settings to be saved through the browser.
- Do not accept private Nostr keys in the interface.

## 2. Existing capabilities and gaps

The following is based on source inspection, not a runtime verification of these features:

| Area | Existing foundation | Required addition |
| --- | --- | --- |
| Blobs | In-memory index, metadata, local/S3 storage, authorized deletion, pubkey-scoped listing | Global admin listing, search, filters, details, previews and bulk actions |
| Reports | Signed BUD-09 reports, JSON records in `reports/`, automatic quarantine/deletion in applicable modes | Review queue, grouping, explicit review decisions and recovery of interrupted processing |
| Configuration | CLI/env/dotenv parsing and an offline `.env` editor at `/config` | Source-aware settings view, authenticated writes and managed-file persistence |
| Operations | Metrics, WoT and upstream diagnostics, periodic cleanup | Unified health view, explanations and links to relevant settings/actions |
| Cashu | Bearer-protected balance, withdrawal, withdrawal status and reclaim | Integration with the admin access model and interface |
| Admin access | Wallet-specific bearer credential | Dedicated NIP-98 authorization and administrator pubkeys |

Relevant implementation locations: `src/config.rs`, `src/lib.rs`, `src/models.rs`, `src/middleware.rs`, `src/handlers/delete.rs`, `src/handlers/report.rs`, `src/handlers/admin_wallet.rs`, `src/services/file_storage.rs` and `src/config-editor.html`.

Important constraints:

- `/config` currently edits/imports/exports text in the browser; it does not update the running server.
- Configuration values are copied into `AppState`, and background jobs clone that state. Generic hot reload does not exist.
- Current public reports are recorded as non-destructive signals. A report processing status is not the same as a human review decision.
- Local quarantine retains bytes outside the served tree. The current S3 quarantine path deletes the object instead; restoration is not available there.
- The existing Blossom allowlist also supplies WoT roots. It must not become an administrator list implicitly.

## 3. Information architecture and workflows

The admin interface is an operational tool: prioritize scanability, safe actions and clear state over decorative dashboards. Proposed entry point: `/admin/`.

### Overview

Show storage usage and limits, blob counts, outstanding reports, backend availability and pending configuration changes. Distinguish zero, unavailable, disabled and stale data. Every warning should lead to its relevant list or setting. Add historical charts only if historical data is actually collected.

### Blobs

- Cursor-paginated global listing; search by full hash and pubkey.
- Filter by media type, size, age, origin, backend, expiration and moderation state where data exists.
- Compact table plus optional media view; details show hash, URL, type, size, timestamps, known uploader pubkey, origin, backend, retention and related reports.
- Do not invent original filenames or ownership histories that Almond does not store.
- Single and bounded bulk actions, with explicit selection scope, confirmation and per-item outcomes. A partially failed batch is not overall success.
- Distinguish removing a stored copy from blocking a hash against future uploads, mirrors and upstream fetches. Deletion alone is not a permanent ban.
- Explain effects on paid retention before removal; define moderation precedence without implying an automatic refund.

### Reports and quarantine

Group reports by blob while preserving each original signed event, reporter, reason and receipt/processing information. Human review states should be separate from ingestion states: open, dismissed or action taken; interrupted automatic processing must remain visible.

Review actions: dismiss, quarantine, restore where reversible storage exists, permanently delete, and block a hash when intended. Record the actor, reason and actual result. A queued report can refer to a blob that has already disappeared.

Do not auto-play or automatically reveal potentially harmful content. Fetch privileged previews through an authenticated path; do not make quarantine public to simplify previewing. Never render untrusted HTML/SVG as executable content in the admin origin. Restoration needs an explicit backend implementation, conflict handling and preservation of required metadata; it cannot be promised by a button alone.

### Access and policies

Show administrator keys separately from Blossom access rules. Support collaborator keys, allowlists, WoT/DVM configuration and feature-access modes. Explain why a pubkey is allowed or denied without presenting Nostr identity as a conventional account.

### Settings and operations

Show current values, provenance, editability, saved changes and restart requirements. Cover Almond's operational settings, including retention, size/count limits, uploads, mirrors, upstreams, reports, discovery, access rules and paid operations. Deployment-owned settings remain visible but cannot necessarily be edited in the browser.

Expose backend and job health where measurable. Do not introduce a remote shell, arbitrary filesystem browser or general host-management interface.

### Wallet and activity

Show Cashu controls only when the feature and wallet are enabled. Preserve withdrawal request identifiers, retry safety, status lookup and reclaim behavior. Never log or casually expose exported ecash, bearer credentials or backend secrets.

Provide an activity view for admin mutations and moderation decisions. Logs must identify intent, actor, target and outcome without claiming that an interrupted action completed.

### UI states and delivery constraints

Support disconnected or unavailable signers, rejected signatures, unauthorized keys, loading, empty results, backend outages, stale revisions, partial failures and read-only configuration. Recheck permissions on every request, including after collaborator revocation.

Use keyboard-accessible controls, visible focus, labelled status messages and confirmations that identify the action and target. Desktop supports dense inspection; mobile must still support signing, report triage and safe individual actions. Confirm responsive behavior with real previews before delivery. Frontend stack and detailed visual direction remain open; no separate frontend server is required by this concept.

## 4. Authentication and administrator authorization

### NIP-98 HTTP authentication

Use [NIP-98](https://github.com/nostr-protocol/nips/blob/master/98.md) events in `Authorization: Nostr <base64-event>`. These events authenticate HTTP requests and are not published to relays. [NIP-07](https://github.com/nostr-protocol/nips/blob/master/07.md) and [NIP-46](https://github.com/nostr-protocol/nips/blob/master/46.md) connect the browser to a signer; they are not replacements for NIP-98.

For every protected request, validate the event signature and ID, kind `27235`, a short timestamp window, exact absolute URL including query parameters, HTTP method, and membership in the current admin key set. For requests with a body, require and verify the SHA-256 `payload` tag even though the NIP makes payload binding optional.

Protect mutations against replay. A bounded in-memory event-ID cache covers one process lifetime, not a restart. Before enabling destructive or financial requests, choose a restart-safe policy: durable short-lived replay records or a fresh server challenge invalidated on restart. This detail is a security requirement, not a guarantee supplied by NIP-98 itself. Replay protection also does not replace business-level idempotency for uncertain request outcomes.

Use HTTPS outside local development/tunnelling. Validate the request URL against a configured public origin; do not trust arbitrary `Host` or forwarded headers behind proxies. Keep admin responses and secrets out of caches. CORS is not authorization: every admin handler needs server-side checks.

### Keys, bootstrap and recovery

Proposed external setting: `--admin-npubs` / `ALMOND_ADMIN_NPUBS`. These root keys can only be changed through deployment configuration, not through the managed file or browser.

- No root keys: administration is disabled; no public first-user registration.
- Optional collaborator keys are stored in the managed file and have full admin access, including collaborator management.
- Browser changes cannot remove or replace externally configured root keys.
- Persist collaborator changes before publishing a new in-memory authorization snapshot; revocation affects subsequent requests immediately. Already admitted operations may finish.
- Recovery: configure a new root key locally and restart. No email or user database required.
- Without writable managed storage, collaborator membership is externally managed as part of the root list.

Nostr is the proposed required authentication method for the admin surface, not a new requirement for public Blossom operations. Do not introduce password login or a new session database without a concrete need. Test signing frequency and permission prompts with real browser and remote signers before finalizing the interaction.

Existing wallet bearer endpoints need an explicit cutover: move the UI and operator clients to the common admin authorization before removing obsolete wallet-only credentials. Do not accidentally retain a weaker bypass or silently break existing clients.

## 5. Configuration ownership and precedence

The browser is an editor of permitted settings, not a higher-priority configuration authority.

| Priority, highest first | Source | Browser behavior |
| --- | --- | --- |
| 1 | Explicit CLI argument | Read-only; identify the argument |
| 2 | Process environment | Read-only; identify the variable |
| 3 | Existing dotenv file (`--config` or `.env`) | Read-only; identify the file/key |
| 4 | Optional managed settings file | Editable, subject to field policy |
| 5 | Built-in default | Editable by creating a managed value |

A higher-priority explicit value locks the field even if it equals the default or managed value. List values replace lower-priority lists; they are not implicitly concatenated. Admin roots plus collaborators are an explicitly separate authorization union, not generic configuration merging.

Example: `ALMOND_STORAGE_MAX_SIZE=100GiB` locks the storage limit. The UI explains where to change it; the REST API rejects attempts to change its effective value. An unset retention setting can be saved through the browser and will survive restarts.

### Managed file

Proposed opt-in: `--managed-config /var/lib/almond/settings.json`, with a corresponding env setting following Almond's existing naming convention. The path itself is externally configured. Do not add implicit file discovery or write to the existing `.env`.

- Store only explicit managed values, collaborator pubkeys and a schema version, not a snapshot of defaults or deployment secrets.
- Missing setting means inherit; an empty list means explicitly empty. Use explicit set/reset operations so empty, false and zero are not mistaken for missing.
- Reset removes the managed entry; it does not overwrite a higher-priority source.
- A newly deployed external value may shadow an older managed value. Preserve and display that shadowed value; allow its explicit removal, but never imply it is effective. Removing the external value later can expose it again.
- No configured managed path: settings are read-only, while authorized blob/report/wallet operations remain available.
- An explicitly configured missing file may be initialized on the first valid write. Corrupt, unreadable or unsupported existing files must produce an actionable error, not silently fall back and potentially remove access or moderation rules.
- Use a persistent directory mount for containers. Atomic replacement may not work with a single-file bind mount; surface an explicit error rather than a non-atomic fallback.

Infrastructure remains externally owned initially: listener/TLS, public origin used for auth, config and storage paths, S3 connection/secrets, wallet location, metrics credential and root admin keys. A backend change is not a storage migration. Define a field policy for each other setting rather than assuming every field can be safely changed in the UI.

### Resolution and provenance

Keep one typed `Config` definition, parser and cross-field validator. Preserve source information during resolution. Today `load_env_file()` injects dotenv values into the process environment before clap parses them; the final config alone cannot distinguish file values from genuine environment variables.

Refactor loading to retain this distinction, including legacy aliases and the existing empty-value rules. Resolve candidates without mutating the process environment inside an async request. Reuse schema/editor conventions without introducing an independently drifting third list of settings.

For each field, expose the running value, running source, saved/next-start value, its source, shadowed managed value if any, editability/reason and restart requirement. Redact secrets while indicating whether they are configured. A browser cannot know future environment or CLI changes; the next-start preview assumes the current deployment inputs remain unchanged.

### Safe writes and conflicts

1. Authenticate and authorize, enforce body/field limits, then serialize writes within the process.
2. Re-read the managed file and compare its content revision with the submitted base revision. Reject stale writes rather than overwriting another admin or a detected manual edit.
3. Apply the requested set/reset operations; reject unknown, externally locked or infrastructure-only fields.
4. Validate the complete resolved candidate with the existing typed validation rules.
5. Write a restrictive-permission temporary file in the same directory, flush it, atomically replace the target and flush the directory where supported.
6. Return the saved revision and explicit application state. Publish immediate access changes only after durable storage succeeds.

Include the base revision in the signed request body, not only `If-Match`: NIP-98 does not bind arbitrary headers. Revision comparisons should detect actual file-content changes, not trust a manually maintained counter alone.

Assume one Almond writer per managed file. A process mutex and content revision do not eliminate the race with an arbitrary external editor between comparison and replacement. Document that limitation; require stopping Almond for manual recovery edits. Do not advertise distributed configuration or multi-instance writes.

### Applying changes

Persisted server settings initially take effect on restart. Show running and saved values side by side; saving must not claim to have applied them. No automatic self-restart endpoint is required: the existing supervisor/deployment owns process lifecycle.

Collaborator access is an intentional live-update exception. Any additional live-update behavior needs a field-specific implementation that updates request paths and relevant jobs consistently; changing a struct field or rebuilding all state is not sufficient.

## 6. Administrative REST surface

Proposed namespace: `/admin/api/`. Preserve standard Blossom endpoints and their protocol behavior. The following is a capability inventory, not a frozen endpoint contract:

| Capability | Proposed interface |
| --- | --- |
| Identity and feature availability | `GET /admin/api/me` |
| Overview and health | `GET /admin/api/status` |
| Global blob search | `GET /admin/api/blobs` with bounded filters and cursor |
| Blob detail / privileged preview | `GET /admin/api/blobs/{hash}` and `/preview` |
| Removal / moderation actions | Bounded, explicit action requests with target hashes and reason |
| Reports and human decisions | Paginated report queries and explicit decision requests |
| Configuration | `GET /admin/api/settings`, `PATCH /admin/api/settings` |
| Collaborators | Read/update requests with a signed base revision |
| Activity | Cursor-paginated admin activity queries |
| Wallet | Existing balance/withdrawal/status/reclaim capabilities under common authorization |

A settings mutation can use `{"base_revision":"…","set":{"upload_max_age":"14d"},"reset":["upstream_servers"]}`. Exact field identifiers must follow the chosen shared schema, not a new ad hoc naming scheme.

Use consistent machine-readable errors: `401` invalid authentication, `403` unauthorized key, `409` source/policy conflict, `412` stale revision and `422` invalid candidate settings. Missing preconditions, backend failures and disabled capabilities also need distinct documented responses. No mutation should be accepted through GET.

Return per-target results for bulk operations. Retries of wallet operations preserve existing business request IDs. For other mutations, define queryable outcomes or safe retry semantics; a lost response must not cause the interface to assume success or blindly repeat an unsafe action.

## 7. Database-free operational persistence

Use the existing blob index and report records rather than duplicating them into an admin database. Additional durable state is still necessary; “no database” does not mean “no state”.

- Managed configuration and collaborator keys: one small JSON document.
- Human moderation decisions and hash blocks: file-backed records with explicit schema, atomic replacement and in-memory lookup where needed. A block must cover upload, mirror and upstream paths and survive restart.
- Admin activity: bounded/rotated append-only records with interrupted-action detection. Do not call an ordinary local log tamper-proof or transactionally coupled to storage mutations.
- Authentication replay data: depends on the restart-safe choice in section 4; never retain private keys or signed bearer material in ordinary logs.

Define recovery for crashes between intent, backend mutation and outcome recording. Existing report files are not a transactional journal; do not silently claim stronger evidence guarantees. Before implementation, settle record formats, retention, disk-full behavior and which mutations must fail closed when an intent cannot be recorded.

The moderation API journals each target before its mutation under `admin-activity`, then writes a separate outcome. `GET /admin/api/activity` paginates intents and reports a missing outcome as `uncertain`; operators must inspect the actual blob, quarantine, block and review state before deciding whether another action is appropriate. It is not proof that the mutation failed. Retention keeps at most 10,000 intents, removing oldest completed pairs first; unresolved intents are retained and the API fails closed when they fill the limit. A failed intent write prevents the action. An outcome or review write failure leaves an uncertain intent. These local records are not tamper-proof or transactionally coupled to backend operations.
After inspection, `POST /admin/api/activity/{id}/reconcile` with `{"reason":"observations"}` records the operator and observations as a reconciliation, without asserting that the original operation succeeded or failed. Only an intent without an outcome can be reconciled; the signed request body must describe the evidence used.

Quarantine retains a metadata record on the local storage volume and bytes either in the local quarantine directory or the S3 `.almond-quarantine/` namespace. Both the local volume and S3 bucket must persist across restart. Restore rejects blocked hashes and conflicting destinations; an interrupted move may be retried after inspecting the stored copies. Metadata from older quarantines that lack a matching record cannot be restored automatically. Moderation overrides paid retention and does not automatically refund payments.

Blob listing accepts `mime_type`, `min_size`, `max_size`, `created_after`, `created_before`, `origin`, `backend`, `expiration` and `moderation` filters in addition to search and cursor. Report queries group signed reports by requested hash; ingestion status remains separate from each hash's human reviews.

## 8. Delivery sequence and acceptance

These are implementation slices of the full scope, not a reduced product commitment:

1. **Configuration resolution and admin authorization:** source-aware config, opt-in managed path, root/collaborator model, NIP-98 validation and protected handler foundation.
2. **Read-only operational view:** signer connection, overview, global blob browsing/details, reports and current settings with truthful feature/backend states.
3. **Administrative actions:** removal, moderation decisions, reversible quarantine where supported, hash blocking, bulk results and activity persistence/recovery.
4. **Managed settings and access:** validated atomic writes, revision conflicts, provenance UI, reset/shadow behavior, restart visibility and immediate collaborator revocation.
5. **Wallet integration and completion:** migrate existing operator workflows, exercise remote signing, responsive/accessibility checks and deployment/recovery documentation.

Verify observable behavior, including:

- Valid admin signatures work; bad signatures, wrong URL/query/method/body, expired events, unauthorized keys and mutation replays are rejected. Exercise proxy URLs and restart behavior.
- Root keys cannot be removed in the browser; collaborators have agreed full access, and revoked keys fail subsequent requests.
- CLI/env/file precedence remains intact; explicit false/zero/empty-list values, legacy names, defaults and locked fields resolve correctly.
- Managed values survive restart; reset and shadowing behave as displayed. Invalid or stale writes leave the existing file unchanged.
- Read-only mounts, disk-full/write failures, unsupported files and interrupted saves are reported without false success.
- Blob actions affect the actual backend; failures stay visible. Local and S3 quarantine capabilities are not conflated, and blocked hashes cannot re-enter through sibling paths.
- Human report state and automatic processing state remain distinct; interrupted actions can be reconciled.
- Existing wallet retry/reclaim safety and no-Cashu builds remain functional.
- Run real browser flows with browser/remote signers at desktop and mobile sizes. Tests alone do not establish signer usability or preview safety.

## 9. Decisions still required

- Frontend: use Applesauce for Nostr signer integration, including NIP-07 browser signers and NIP-46 remote signers. Use task-oriented navigation and the existing Almond identity as design evidence.
- Test signing frequency and permission prompts with real browser and remote signers before finalizing the interaction.
- Choose restart-safe replay protection and recovery of uncertain mutation outcomes.
- Define the final field editability policy and exact managed-file schema/path options.
- Implement reversible S3 quarantine; define restore semantics and handling of existing quarantined files with incomplete metadata.
- Moderation takes precedence over paid retention without an automatic refund. Define hash-block scope and activity/report retention.
- Replace existing wallet bearer authorization with common admin authorization as a documented breaking cutover; define exact API contracts and operator migration instructions.

Do not add a user database, role framework, generic hot reload, automatic restart manager, new storage backend or plugin system to answer these questions.
