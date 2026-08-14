# Known Blossom Specification Issues

Scope: current implementation compared with Blossom BUD-00 through BUD-12 from
[`hzrd149/blossom`](https://github.com/hzrd149/blossom), reviewed 2026-08-13.

This document distinguishes protocol deviations from intentionally unsupported
optional BUDs. A BUD marked optional does not make its absence a mandatory
protocol violation.

## Protocol deviations

### BUD-01 — CORS is not literally present on every response

**Requirement:** `Access-Control-Allow-Origin: *` on all responses.

**Current behavior:** `src/middleware.rs:is_internal_path` treats `/config`,
`/metrics`, `/_metrics`, `/_wot`, `/_upstream`, and `/filter-test.html` as
operator-private. Those responses either echo only an allowlisted origin or
return no CORS header.

**Impact:** Browser clients cannot cross-origin read these routes. This is an
intentional operator-surface policy, but it is a literal deviation if BUD-01's
"all responses" wording is applied to the entire origin.

**Resolution:** Either serve `Access-Control-Allow-Origin: *` universally or
state that these routes are outside the Blossom API surface and host them on a
separate origin.

### BUD-01 — Redirect targets are not validated for required CORS and metadata headers

**Requirement:** A redirect destination must provide `Access-Control-Allow-Origin:
*`, `Content-Type`, and `Content-Length`.

**Current behavior:** `src/handlers/upstream.rs:try_head_and_redirect` checks
only that an upstream `HEAD` response is successful, then
`build_redirect_response` returns its URL unchanged.

**Impact:** Redirect-mode downloads can lead to an origin that browsers cannot
read cross-origin or that does not expose a usable MIME type/length.

**Resolution:** Proxy upstream bytes, or only redirect after validating those
headers and using a controlled URL whose extension accurately represents the
content type.

### BUD-04 — Mirror fallback does not inspect content when the origin omits Content-Type

**Requirement:** When an origin lacks `Content-Type`, the server **SHOULD**
detect MIME type from the bytes and extension, falling back to
`application/octet-stream` only if still unknown.

**Current behavior:** `src/handlers/upload.rs:mirror_blob` calls
`extract_content_type_from_response`; absent input immediately becomes
`application/octet-stream`.

**Impact:** Mirrored content can receive a generic descriptor MIME type even
when its bytes or URL extension identify it.

**Resolution:** Run bounded MIME detection after the streamed download when the
origin has no usable content type.

### BUD-07 — X-Cashu 402 challenges are not NUT-24 payment requests (paid mode)

**Requirement:** A server using `X-Cashu` **MUST** follow NUT-24.

**Current behavior:** `src/error.rs:AppError::into_response` serializes a JSON
object and labels its Base64url encoding `cashuA`. NUT-24 requires an HTTP 402
payment request in `creqA` (Base64url CBOR) or `creqB` form.

**Impact:** NUT-24 wallets cannot decode the challenge and therefore cannot
complete paid upload, mirror, or download flows interoperably.

**Resolution:** Construct the NUT-24 `PaymentRequest` type and emit the
specified `creqA`/CBOR representation. Keep accepting `cashuB` for retries as
NUT-24 requires.

### BUD-07 — A settled but insufficient payment returns 402 instead of 400 (paid mode)

**Requirement:** Failed payment proof, including an insufficient payment, **MUST**
return `400 Bad Request` with `X-Reason`.

**Current behavior:** `src/services/cashu.rs:charge` returns a new
`PaymentRequired` error when `received_sats < quoted.amount_sats`; the response
is `402`.

**Impact:** Clients cannot distinguish a missing payment challenge from a proof
that was supplied but did not satisfy the quote.

**Resolution:** Return `AppError::BadRequest` with an `X-Reason` after settlement
proves the amount is short.

### BUD-09 — Public reports can succeed without a blob `x` tag

**Requirement:** A `/report` body **MUST** be a signed NIP-56 event with one or
more blob-hash `x` tags.

**Current behavior:** `src/handlers/report.rs:report_blob` returns `202 Accepted`
for `FEATURE_REPORT_ENABLED=public` before it extracts and validates `x` tags.

**Impact:** A signed kind-1984 event without a target is accepted as a report.

**Resolution:** Extract and validate at least one lowercase SHA-256 `x` tag
before every successful response, including non-destructive public mode.

### BUD-09 — No published report rules or terms route

**Requirement:** Servers **SHOULD** advertise rules or terms affecting reports.

**Current behavior:** The public routes in `src/main.rs:create_app` contain no
report policy/terms endpoint, and the homepage does not publish report rules.

**Impact:** Reporters cannot determine moderation scope, operator policy, or
whether reports are retained/actioned.

**Resolution:** Publish a stable policy URL from the homepage or add a dedicated
report-policy endpoint.


### BUD-11 — Authorization token content is not machine-verifiable

**Requirement:** Authorization event `content` **MUST** be a human-readable
string explaining intended use.

**Decision:** Almond does not infer whether arbitrary Unicode text is
human-readable or explains an action. BUD-11 defines neither a grammar nor a
language-independent predicate for that semantic claim; its example event also
uses empty `content`. Empty or non-explanatory content therefore remains a
client-side protocol responsibility.


### BUD-12 — /list/<pubkey> is not an uploader-specific list

**Requirement:** `/list/<pubkey>` **MUST** return blobs uploaded by the specified
public key.

**Current behavior:** `FileMetadata.pubkey` is always absent, and
`src/handlers/list.rs:list_blobs` treats any `ALLOWED_NPUBS` member as the same
operator catalogue; other keys receive an empty list.

**Impact:** Results do not represent uploads by the requested pubkey.

**Resolution:** Persist uploader pubkeys during upload/mirror authorization,
backfill or version existing metadata, index by pubkey, and query that index for
`/list/<pubkey>`.

### BUD-12 — DELETE returns 204 for an absent blob

**Requirement:** `404 Not Found` is the recommended response when a blob does
not exist or is unavailable for deletion.

**Current behavior:** `src/handlers/delete.rs:delete_blob` ignores the boolean
result from `remove_indexed_blob` and always returns `204 No Content` after a
valid authorization.

**Impact:** Clients cannot distinguish deletion of an existing resource from an
idempotent no-op. This is a **SHOULD** status-code deviation, not a missing
endpoint.

**Resolution:** Return `404` when no indexed or native copy existed, or document
idempotent-delete semantics as an intentional compatibility choice.

## Unsupported optional BUDs

### BUD-05 — Media processing

`PUT /media` and `HEAD /media` are not routed in `src/main.rs:create_app`.
Media transformations/conversions are therefore unavailable. BUD-05 is optional;
this is a declared unsupported capability rather than a mandatory violation.

### BUD-10 — blossom: URI handling

BUD-10 specifies client-side parsing, generation, and resolution of `blossom:`
URIs. Almond provides no client URI API. This is not a server-protocol violation,
but the capability is unavailable in this repository.

## Not issues

- BUD-03 is client-side server-list behavior; no server implementation is
  required.
- BUD-08 `nip94` is optional and is returned in current blob descriptors.
- `Sunset`, `HEAD /upload`, range responses, descriptor extension fallback,
  quarantine re-upload blocking, BUD-11 list/delete tags, and the required
  CORS behavior on Blossom API routes are implemented.
