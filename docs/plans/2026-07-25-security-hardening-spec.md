# Security Hardening Specification

**Status:** Audit result; implementation not started  
**Date:** 2026-07-25  
**Scope:** HTTP API, blob storage, upstream/mirror paths, chunked uploads, authentication, Cashu and deployment

## Goal

Almond's publicly reachable instance must not be brought into an erroneous, exhausted or
destructive state either by individual HTTP requests or by cheap request loops. In
particular, unregistered attackers must not:

- consume RAM, disk, inodes, file descriptors or CPU without bound;
- reach internal services via mirror/upstream fetches;
- block other users' uploads or trigger destructive actions;
- bypass an enabled payment feature;
- read sensitive operational and local metadata from browser contexts.

## Baseline and threat model

The current default configuration is functionally public:

- `FEATURE_UPLOAD_ENABLED=public`;
- `FEATURE_MIRROR_ENABLED=public`;
- `ALLOWED_NPUBS` is empty;
- the Docker image binds to `0.0.0.0:3000`.

In `public` mode a valid Nostr signature is required, but any attacker can generate
their own key pair. In this specification **unregistered attacker** therefore means: a
client with a self-generated, validly signed Nostr event.

The findings are a source-code-based analysis. Findings marked as dependent on feature
flags apply only if the respective feature has been activated. A dynamic penetration
test against a running production instance was not part of this analysis.

## Protection invariants

1. Every incoming request has a transport-effective, endpoint-appropriate body limit.
   Streaming code enforces the same limit itself.
2. Temporary files and incomplete uploads are fully quota-accounted, bounded and
   deletable on all error paths.
3. No URL fetch may reach a private, local, link-local or otherwise non-publicly
   routable destination after DNS resolution or redirect.
4. Chunked uploads belong to exactly one pubkey; another pubkey can neither modify nor
   block their state.
5. Public read/discovery endpoints have bounded computational and memory complexity per
   request.
6. Destructive operations require at least the same authorization level as
   `DELETE /:filename`.
7. Enabled payment paths do not permit work or persistence above the paid size.
8. Infrastructure and diagnostic endpoints are not uncontrollably readable from a public
   browser context.

## Priority 0 — fix before public operation

### P0-1: Enforce a real request body limit

**Finding:** `DefaultBodyLimit` in `src/main.rs:126-129` only affects extractors that
apply it. `upload_file`, `mirror_blob` and `patch_upload` on the other hand take
`Request<Body>` and consume `req.into_body()` directly (`src/handlers/upload.rs:101`,
`:177`, `:436`).

`stream_to_temp_file` (`src/services/upload.rs:184-235`) writes without its own size
check. An endless `PUT /upload` can therefore fill the partition via
`files/temp/upload_<uuid>`. Hash authorization only happens after streaming.

**Requirement:**

- Deploy a transport-effective `RequestBodyLimitLayer`.
- Use a separate, small limit for `/mirror`; the JSON body may be at most 64 KiB.
- `stream_to_temp_file` must receive `max_bytes`, count written bytes and abort with
  `PayloadTooLarge` on exceedance.
- Introduce a maximum total blob size as configuration and enforce it in the regular as
  well as the chunked upload path.
- Check a configurable free-disk reserve before writing; the check complements but does
  not replace the byte limit.

**Acceptance criteria:**

- A chunked transfer over the endpoint limit receives `413`; the temp file path no
  longer exists afterwards.
- An infinite or very slow upload connection can neither write more than the limit nor
  permanently hold a temp file descriptor.
- The result also applies to handlers that use `Request<Body>`.

### P0-2: Remove unbounded mirror buffering

**Finding:** `mirror_blob` uses `axum::body::to_bytes(req.into_body(), usize::MAX)`
(`src/handlers/upload.rs:177-179`). A large, chunked request is read fully into the heap
even though only `{"url":"..."}` is expected.

**Requirement:**

- Limit the mirror body to 64 KiB or use a bounded `Json` extractor.
- Exceedance must be `413 Payload Too Large`.
- Parser error details must not log the full, attacker-controlled body.

**Acceptance criterion:** A multi-gigabyte or endless body for `PUT /mirror` does not
consume more than the endpoint limit in the heap and does not terminate the process.

### P0-3: Close SSRF via redirects and rebinding

**Finding:** `validate_url_for_ssrf` only validates the initial URL
(`src/services/upload.rs:53-135`). The clients follow redirects up to five times
(`src/services/upload.rs:137-151`, `:153-181`) without re-validating the redirect
target. Thus a public HTTPS source can redirect to an internal target. Furthermore the
initial resolution and the subsequent client connection are not pinned to the same IP.

**Requirement:**

- Disable redirects by default for all SSRF-sensitive fetches.
- If redirects are needed: parse, normalize, resolve DNS, validate against the same
  policy each hop at most once, and keep the hop count small.
- The connection used must be pinned to a previously validated public IP; a pure upfront
  resolution is not sufficient.
- The IP policy must handle IPv4, IPv6, IPv4-mapped IPv6, loopback, unspecified,
  link-local, RFC1918, carrier-grade NAT and other non-public reserves in a fail-closed
  manner.
- DNS timeouts must not exhaust the shared Tokio blocking pool. Use an asynchronous
  resolver or a strictly separated, bounded DNS pool.

**Acceptance criteria:**

- A public HTTPS URL with a redirect to `127.0.0.1`, `::1`, an IPv4-mapped loopback
  address, `169.254.169.254` or an RFC1918 address is not requested.
- A hostname that changes its IP after successful validation does not reach a private
  target address.
- Many deliberately delayed DNS resolutions do not block blob read or write operations.

### P0-4: Do not make public report destructive

**Finding:** With `FEATURE_REPORT_ENABLED=public` `PUT /report` accepts any validly
self-signed kind-1984 event (`src/handlers/report.rs:112-127`). With
`REPORT_ACTION=delete` or `quarantine` any known blob hash can be deleted or made
unusable (`:196-224`).

**Requirement:**

- `public` must not trigger a direct file action for reports.
- Direct deletion requires at least the existing strict whitelist authorization.
- Quarantine via reports must either also be strictly authorized or be persisted as a
  moderation-requiring, non-destructive notice.
- Report events need a short, verified validity and replay protection.

**Acceptance criterion:** A self-signed public report event cannot delete, move or
remove another user's blob from the index.

## Priority 1 — Availability and integrity

### P1-1: Bound chunked uploads and bind them to an owner

**Findings:**

- `X-SHA-256` is not checked for exactly 64 lowercase hex characters in `PATCH /upload`
  (`src/handlers/upload.rs:327-330`). The value flows into a map key and into chunk file
  names (`:423-429`, `:479-492`).
- `chunk_uploads` is indexed solely by hash (`src/models.rs:297`), not by the
  authenticated pubkey.
- `Upload-Length` has no upper bound.
- `upload_offset + content_length` can overflow (`src/handlers/upload.rs:405-411`).
- The map has no capacity limit. Empty chunks still create files and `sync_all()`.
- The global write lock is held during a Cashu `await`
  (`src/handlers/upload.rs:479-561`).
- Parallel completion requests can reconstruct the same upload simultaneously; error
  paths leave `reconstruct_*` temp files behind.

**Requirement:**

- Apply `file_storage::validate_sha256_format` immediately after header parsing and
  accept only lowercase.
- Index upload state by `(PublicKey, sha256)` instead of `sha256` alone.
- Set a maximum number of parallel sessions globally, per pubkey and per source IP.
- Check `Upload-Length` against the new maximum blob size.
- Use `checked_add` for offset plus length.
- Claim completion atomically: remove the upload state from the map under lock or
  transition it into a state that cannot be completed again, before reconstruction
  begins.
- No network I/O while holding the `chunk_uploads` lock.
- Delete chunk files on every error; secure reconstruction files via RAII guard and
  extend cleanup to `files/temp/`.
- Do not force `sync_all()` for every empty or tiny chunk; one defined, documented
  durability point suffices.

**Acceptance criteria:**

- Another pubkey cannot block or modify an upload with the same hash.
- Many 0-byte PATCH requests lead neither to unbounded map entries nor to uncontrolled
  inode consumption.
- Two parallel completion attempts produce exactly one reconstruction and no remaining
  `reconstruct_*` file.
- Overflowing header values yield `400`, never a panic or state mutation.

### P1-2: Make public index and filter endpoints scalable

**Findings:**

- `/list` clones and sorts the entire index before pagination
  (`src/handlers/list.rs:263-296`, `src/services/blob_index.rs:131-140`).
- `/filter` accepts arbitrarily many `fp` values as cache key; with Binary-Fuse the
  value is not even relevant to the output but forces a rebuild.
- `/_wot` rebuilds a Bloom filter for each request; `fp=NaN` causes a Bloom filter
  assert (`src/handlers/wot.rs:28-37`).
- `failed_upstream_lookups` is a capacity-less map that lives for hours
  (`src/handlers/file_serving.rs:330-336`, `src/utils.rs:437-444`).

**Requirement:**

- `BlobIndex` must provide a paginated view sorted by `(created_at, sha256)` that clones
  at most the requested page size.
- `/filter` must remove `fp` from the cache key for Binary-Fuse; single-flight filter
  rebuilds and discretely bound FP values.
- `/_wot` must reject non-finite FP values with `400`, allow only fixed FP tiers and
  cache results generationally.
- Keep negative lookups in a capacity-bounded LRU or disable them entirely when
  upstreams are absent.
- All compute-intensive public endpoints receive request and rate limits.

**Acceptance criteria:**

- `GET /list?limit=1` does not allocate proportionally to the total blob count.
- Parallel cache misses for the same filter produce only one filter build.
- `GET /_wot?fp=NaN` returns `400` without a panic log.
- Random hash requests do not grow the negative cache beyond the configured capacity.

### P1-3: Enforce storage quota correctly and synchronously

**Finding:** `enforce_storage_limits` sorts ascending by age and keeps the oldest files
first (`src/utils.rs:158-205`). When storage is full, new uploads are initially
confirmed as successful and deleted later. Enforcement happens only periodically;
temporary files are not counted.

**Requirement:**

- Explicitly define the desired eviction model. For FIFO-like retention the newest
  objects must be preserved and the oldest evicted.
- Alternatively reject writes before persisting with `507 Insufficient Storage`.
- Final blobs, chunk files, reconstruction files and HLS temp files must be accounted
  for in the available disk reserve and quota.
- The check must happen on the write path in addition to the periodic cleanup.

**Acceptance criterion:** A fully occupied store does not confirm an upload with `201`
that disappears in the next cleanup run.

### P1-4: Bound HLS mirror and clean up

**Findings:**

- Error paths in `src/services/hls.rs` create futures of `remove_file` without awaiting
  them; the temp files persist.
- Playlist references are not globally bounded; a large playlist can generate very many
  outgoing requests.

**Requirement:**

- Delete temp files via RAII or guaranteed async cleanup on all error paths.
- Set a maximum playlist size, maximum references per round, maximum total references
  and global deduplication.
- Execute HLS fetches with bounded parallelism and a total budget.

## Priority 2 — Authentication, payment and data protection

### P2-1: Actually enforce the upload whitelist

**Finding:** `FeatureMode::Public` maps to `AuthMode::Unrestricted`. Therefore a set
`ALLOWED_NPUBS` alone does not limit uploads.

**Requirement:**

- Introduce an explicit whitelist mode or map `public` to a fail-closed authorization
  when `ALLOWED_NPUBS` is non-empty.
- Documentation and configuration examples must explain the actual mode.

### P2-2: Make tokens short-lived and replay-resistant

**Finding:** `verify_event` checks signature, past and expiry but no maximum TTL, no
maximum age and no replay cache (`src/services/auth.rs:47-81`). When `server` tags are
absent, tokens are intentionally valid across servers.

**Requirement:**

- Set a maximum event TTL and a maximum clock skew.
- For destructive events maintain at least a TTL-based event-ID replay cache.
- Provide an operator option for mandatory server binding.
- Accept BUD-11 Base64url without padding; standard Base64 may be supported as a
  compatible fallback.

### P2-3: Execute Cashu paths fail-closed

**Only applies when `FEATURE_PAID_*` flags are enabled.**

**Findings:**

- Mirror payment is skipped when the upstream does not provide `Content-Length`.
- The price is based on the untrusted `Content-Length`, not on the actually transferred
  bytes.
- The amount actually credited after the swap is not compared against the price.
- The final chunk is saved before a failed 402 payment check; a retry then fails on the
  duplicate check.
- The wallet seed is written with default file permissions.

**Requirement:**

- Determine mirror payment based on the actually streamed byte count or handle missing
  length fail-closed.
- Check the net redeemed amount against the required amount.
- Make the chunked 402 path rollback-capable.
- Create wallet seed atomically with `0600`.
- Either maintain one wallet per accepted mint or configure and advertise exactly one
  mint.
- Map wallet/mint infrastructure errors as 5xx, not as client 400.

### P2-4: Protect browser and operational metadata

**Findings:**

- `Access-Control-Allow-Origin: *` is set on all endpoints (`src/middleware.rs:9-42`).
  This allows foreign websites, especially on local cache instances, to read `/list`,
  `/_wot`, `/metrics` and blob contents.
- `/metrics` and `/_metrics` are public. Upstream hosts can create unbounded Prometheus
  labels when custom origins are enabled.

**Requirement:**

- Restrict the CORS wildcard to public blob GET/HEAD responses.
- Use a configured origin allowlist for metadata and diagnostic paths.
- Serve metrics on a separate admin listener or behind auth.
- Limit upstream metric labels to known hosts or aggregate unknown targets as `other`.

## Platform and operational requirements

1. Incoming HTTP connections need header read timeout, request timeout, a global
   concurrency limit and per-IP rate limits.
2. Docker must run under a dedicated non-root user.
3. The runtime image must migrate from `debian:bullseye-slim` and `libssl1.1` to a
   supported base.
4. The container filesystem must be read-only where possible; only explicit volumes for
   blob and wallet data are writable.
5. `fips.pem` and `fips-key.pem` must be added to `.gitignore`. The files are currently
   not versioned, but not sufficiently protected against accidental commit.
6. `CLEANUP_INTERVAL_SECS=0` must be explicitly rejected or handled safely; a panic in
   the background job must not permanently terminate cleanup.
7. Background jobs must surface errors and panics and be restarted in a controlled
   manner.
8. Establish dependency hygiene in CI: `cargo audit` and `cargo deny`. The bundled
   Cashu/SQLite stack is functional but contains an old bundled SQLite version and
   additionally two Reqwest major versions.

## Non-findings

The analysis did not find a direct path traversal write via the final blob path: final
paths in the checked code paths only receive hashes that were verified against an
actually computed SHA-256 digest. Chunk header validation is nonetheless mandatory
because the raw values affect state and temp file names beforehand.

`DELETE /:filename` itself is fail-closed: it requires a non-empty whitelist, strict
authorization and matching `t=delete` and `x` tags. The report endpoint must not provide
a weaker alternative deletion path.

## Acceptance plan

Implementation is only complete once at least the following tests exist and are green:

1. Bounded and endless bodies for upload and mirror yield `413`; neither disk nor heap
   exceed the configured budget.
2. SSRF redirects and DNS rebinding attempts do not reach non-public targets.
3. Foreign pubkeys cannot block chunk sessions; sessions, chunk files and reconstruction
   files stay within configured limits.
4. Public `/list`, `/filter`, `/_wot` and random-hash GETs stay under parallelism within
   a given CPU/memory budget.
5. Public reports cannot trigger a filesystem action.
6. With paid features enabled, chunked, mirror and download paths are only completed
   after correctly redeemed and sufficient payment.
7. CORS and metrics tests confirm that local/administrative metadata are not readable
   cross-origin or publicly.
8. The container runs non-root; the wallet seed has mode `0600`; no PEM file outside the
   intended secret distribution is versioned.
