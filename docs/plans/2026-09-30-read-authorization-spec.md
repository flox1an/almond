# Blob Read and Upstream Fetch Authorization

**Status:** Decisions made, implementation pending
**Date:** 2026-09-30 (decisions recorded 2026-10-01)

## Purpose

Let an operator restrict every blob GET/HEAD served by Almond, including the
upstream work those requests trigger, to authorized Nostr users. Configuration
is a single new `.env` switch, and the existing Nostr authorization
implementation is reused.

All pending decisions (D1–D7) are resolved; see the
[decision register](#decision-register). This document is the implementation
contract. No code has been changed yet.

## Summary of the chosen behavior

- **Scope (D1, variant A):** When enabled, every blob GET/HEAD requires
  authorization, whatever the source: upload, explicit mirror, native S3,
  serve-files, upstream-cache hit, in-flight download, new upstream fetch, or
  known upstream miss. One gate runs before `resolve_blob`.
- **Configuration (D2):** One new switch, `ALMOND_READ_ACCESS=public|whitelist|wot|dvm`
  (CLI `--read-access`). The default `public` keeps today's anonymous behavior.
- **Access groups (D3):** anonymous, `ALMOND_ALLOWED_NPUBS`, web of trust, announced
  DVMs. The intended deployment is `ALMOND_READ_ACCESS=whitelist`.
- **Errors (D4):** 401 with `WWW-Authenticate: Nostr` when the credential is
  missing or invalid; 403 when the credential is valid but the signer is not
  permitted. This applies to the read path only.
- **Delivery (D5):** Proxy only while protection is active. Redirect upstream
  modes combined with protection are a startup error.
- **Token expiry (D6):** Enforced during streaming. Client streams and followers
  terminate when the token expires. Background cache fills continue.
- **HTTP caching (D7):** Protected responses are sent with
  `Cache-Control: private, no-store` and no `Expires`.

## Current behavior

- `create_app` routes blob GET and HEAD to `handle_file_request` in
  [`src/main.rs`](../../src/main.rs).
- [`src/handlers/file_serving.rs`](../../src/handlers/file_serving.rs) resolves
  indexed blobs, unindexed native S3 objects, serve-files content, recent upstream
  misses, and upstream fallback. It does not authenticate blob reads today.
- Completed uploads and automatic upstream-cache fills share one lookup
  interface. `FileMetadata.origin` distinguishes `Upload` from `UpstreamCache` in
  [`src/models.rs`](../../src/models.rs).
- [`src/handlers/upstream.rs`](../../src/handlers/upstream.rs) supports proxy,
  redirect, and redirect-and-cache modes, including in-flight download followers
  and background cache fills. A cold HEAD in proxy mode can trigger a full GET
  and a background cache fill.
- Local blob responses advertise `public, max-age=31536000, immutable`,
  including HEAD, partial, and 304 responses. Upstream response builders also
  set cache headers.
- [`src/services/authorization.rs`](../../src/services/authorization.rs) owns
  operation policy through `authorize`, `Operation`, and `Authorized.bind`.
  `Authorized` already carries the token's expiration.
  [`src/services/auth.rs`](../../src/services/auth.rs) parses and verifies events
  and applies the pubkey, TTL, and server-binding checks.
- Web-of-trust membership is built from `ALMOND_ALLOWED_NPUBS` as roots
  (`refresh_trust_network`). DVM membership is cached only for positive results.
  An unknown key triggers a live relay lookup (`check_dvm_announcement`) each
  time it is checked.
- The WoT refresh job ([`src/main.rs`](../../src/main.rs)), the DVM refresh job,
  and the `ALMOND_DVM_KINDS` validation ([`src/config.rs`](../../src/config.rs))
  currently look only at upload, mirror, and custom-origin settings.
- `ALMOND_UPSTREAM_MODE` accepts `proxy`, `redirect`, and `redirect_and_cache`.
  Unknown values fall back to `proxy`.
- The outbound header allowlists in [`src/helpers.rs`](../../src/helpers.rs)
  do not forward the incoming `Authorization` header.

## Protocol requirements

Primary sources:

- [BUD-01: Server requirements and blob retrieval](https://github.com/hzrd149/blossom/blob/master/buds/01.md)
- [BUD-11: Nostr Authorization](https://github.com/hzrd149/blossom/blob/master/buds/11.md)
- [Local Blossom Cache profile](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md)
- [RFC 9110: HTTP authentication and status codes](https://www.rfc-editor.org/rfc/rfc9110.html#section-15.5.2)
- [RFC 9111: HTTP caching](https://www.rfc-editor.org/rfc/rfc9111.html#section-5.2.2.5)

1. A server may require a signed authorization event for blob GET and HEAD.
2. Both GET and HEAD use kind `24242` and `t=get`.
3. The header is `Authorization: Nostr <base64url-without-padding event JSON>`.
4. The server verifies the signature and ID, the kind, the creation time, a
   future expiration, the action, and any applicable scope. Event `content` must
   explain the intended use in human-readable text. Existing validation does not
   judge whether arbitrary text is meaningful.
5. GET/HEAD `x` tags are optional. If present, the requested hash must be among
   them.
6. `server` tags are optional. If present, at least one must match Almond's
   domain. `ALMOND_AUTH_REQUIRE_SERVER_TAG=true` additionally makes them mandatory.
7. Read tokens are reusable until they expire. Repeated HEAD, conditional, and
   range requests may present the same token.
8. BUD-01 lists 401 for missing or invalid auth and 403 for policy denial.
   HTTP 401 requires a `WWW-Authenticate` challenge.
9. Blossom CORS responses, including errors, keep
   `Access-Control-Allow-Origin: *`. An OPTIONS preflight allows `Authorization`
   and does not itself require a token.

A deployment with `ALMOND_READ_ACCESS` other than `public` is not a transparent
Local Blossom Cache. That profile requires reads without an Authorization
header.

## Specification

### Configuration (D2, D3, D5)

One new switch, parsed strictly:

| `ALMOND_READ_ACCESS` | Admits | Auth mode reused |
| --- | --- | --- |
| `public` (default) | Anyone, no token required; today's behavior | none (gate inactive) |
| `whitelist` | Valid `t=get` token whose signer is in `ALMOND_ALLOWED_NPUBS` | `AuthMode::Strict` |
| `wot` | Valid token; signer is in `ALMOND_ALLOWED_NPUBS` or in the web of trust | `AuthMode::WotOnly` |
| `dvm` | Valid token; signer is in `ALMOND_ALLOWED_NPUBS` or is an announced DVM | `AuthMode::DvmOnly` |

Startup rules:

- An unknown `ALMOND_READ_ACCESS` value is a startup error, like every other
  enum setting. It must never fall back silently.
- `ALMOND_READ_ACCESS=whitelist` with an empty `ALMOND_ALLOWED_NPUBS` is a
  startup error (an invalid npub is already a startup error).
- `ALMOND_READ_ACCESS=dvm` requires `ALMOND_DVM_KINDS`, like the existing DVM feature
  modes. Extend that validation and the DVM refresh-job condition.
- `ALMOND_READ_ACCESS=wot` must start the WoT refresh job. With an empty
  `ALMOND_ALLOWED_NPUBS` the web of trust has no roots and admits nobody. This fails
  closed; per D2 it is not a startup error, and it must be documented.
- `ALMOND_READ_ACCESS` other than `public` together with `ALMOND_UPSTREAM_MODE=redirect` or
  `redirect_and_cache` is a startup error (D5).
- Existing upload, mirror, custom-origin, and delete configuration and their
  semantics are unchanged.

Proposed `.env.example` block (in the authorization section):

```dotenv
# Blob GET/HEAD access, including cache hits and upstream fetches.
# public:    anonymous access (default, current behavior)
# whitelist: signed t=get event; signer must be in ALMOND_ALLOWED_NPUBS
# wot:       signed t=get event; signer in ALMOND_ALLOWED_NPUBS or its web of trust
# dvm:       signed t=get event; signer in ALMOND_ALLOWED_NPUBS or an announced DVM
#            (requires ALMOND_DVM_KINDS; unknown keys may cause one relay
#            lookup per request)
# Any value other than public requires ALMOND_UPSTREAM_MODE=proxy.
ALMOND_READ_ACCESS=public
```

Intended deployment:

```dotenv
ALMOND_READ_ACCESS=whitelist
ALMOND_ALLOWED_NPUBS=npub1...
ALMOND_UPSTREAM_MODE=proxy
```

### Gate placement (D1)

- One authorization call in `handle_file_request`, after hash parsing and before
  `resolve_blob`. When `ALMOND_READ_ACCESS=public` it returns immediately and changes
  nothing.
- A rejected request performs no index lookup that could publish metadata and
  no native S3 lookup or publication. It does not read or mutate the upstream
  negative cache, and it runs no upstream planning, probe, negotiation, GET,
  in-flight attachment, or background fill.
- The upstream helpers do not get their own caller authentication. They are
  only reachable through the gated handler.
- Metadata, 304, range responses, and 416 are only produced after
  authorization.
- Cashu download charging (where it applies today) runs after authorization.
  Payment behavior is otherwise unchanged; see the caveats below.

### Read authorization (D3, D4)

- Add a non-destructive `Operation::Read` to
  [`src/services/authorization.rs`](../../src/services/authorization.rs). It maps
  the `ALMOND_READ_ACCESS` value to the auth modes in the configuration table.
- `bind` for reads validates `t=get` and optional `x` scoping against the
  requested hash (new `validate_get_auth` in
  [`src/services/auth.rs`](../../src/services/auth.rs)). It consumes no
  single-use nonce. Signature, kind, TTL cap, and server binding reuse
  `verify_event_with_policy` unchanged.
- Status mapping on the read path only:
  - **401** with `WWW-Authenticate: Nostr` for a missing header, malformed
    token, bad signature, wrong kind, token in the future, expired token, TTL
    above the policy, wrong `t`, a non-matching `x`, or a non-matching or missing
    required `server` tag.
  - **403** when the token is valid but the signer is not admitted by the
    selected group. This includes a failed DVM relay lookup, which fails closed
    as today.
- `check_pubkey_authorization` currently returns `Unauthorized` for
  non-members. The read operation maps that to 403. Upload, mirror,
  chunk-upload, list, and delete keep their current codes.
- Add `WWW-Authenticate` to `Access-Control-Expose-Headers`
  ([`src/middleware.rs`](../../src/middleware.rs)) so browser clients can read
  the challenge. Blossom CORS stays `*`, and preflight needs no token.

### Delivery (D5)

While `ALMOND_READ_ACCESS` is not `public`, upstream content is delivered only through
Almond's proxy path. Redirect modes are rejected at startup, so no protected
request can return an upstream URL. Proxying does not make a blob that is
already public upstream private; it keeps delivery through Almond under the gate.

### Token expiry during streaming (D6)

- The token's expiration is the deadline for the response body. Authorization
  at admission requires an unexpired token. After that, the body of every
  protected response stops at the deadline.
- Enforce this once, at the handler's response seam: wrap the final response
  body of an authorized request in a deadline body. This covers local files,
  native S3, upstream proxy streams, and in-flight followers alike, without
  per-source timers.
- When the deadline passes mid-body, the stream ends with an error so the
  connection aborts. It must never end cleanly as a shortened 200 or 206. The
  status line has already been sent, so no 401 can follow.
- Each follower of a shared in-flight download has its own deadline.
  Terminating one reader does not affect other readers.
- Background cache fills are detached and deliver no bytes to clients. They are
  not subject to the deadline and continue to publish hash-verified cache
  entries. A reader aborting does not cancel the fill, which matches the
  existing initiator-disconnect behavior.
- HEAD, 304, 416, and error responses have no body to cut.
- Clients with short-lived tokens must be able to resume. They re-request with a
  fresh token and a `Range` header. Document this for video and HLS clients.

### HTTP caching (D7)

- When `ALMOND_READ_ACCESS` is not `public`, every blob GET/HEAD response is protected.
  This includes HEAD, 200, 206, 304, 416, 401, 403, 404, and other errors. Each
  is sent with `Cache-Control: private, no-store` and without `Expires`.
  Incompatible cache headers from upstream proxy responses are replaced.
- Apply this at one response-finalization point that also covers `AppError`
  responses. Do not change each builder separately.
- ETag, `Accept-Ranges`, content type and length, `Content-Range`, and `Sunset`
  are preserved.
- With `ALMOND_READ_ACCESS=public`, today's headers are unchanged.
- Almond's internal blob store and request coalescing are unchanged. HTTP
  `no-store` concerns HTTP caches, not Almond's own storage.

## Consequences

The implementation and user-facing documentation must state these explicitly:

1. **Embeds stop working.** With protection active, normal `<img>`, `<video>`,
   and HLS embeds without an auth-capable client no longer work for any blob,
   including the operator's own uploads. CORS does not change this.
   Query-string and cookie tokens are out of scope.
2. **DVM relay load.** In `dvm` mode, any unknown key with a valid signature can
   trigger one relay lookup per request, because only positive DVM results are
   cached. This is a deliberate choice (D3). It is documented as a load lever,
   not mitigated.
3. **Streaming expiry is the largest work item.** Streams need deadline
   tracking and a clean abort that can never look like a complete response.
   Players must re-request with a fresh token.
4. **Existing public copies are not recalled.** Responses already cached
   publicly may remain fresh for up to a year: CDNs, reverse proxies, and
   browsers. When switching from `public` to a protected value, purge the caches
   you control. Copies held elsewhere cannot be recalled.
5. **No protocol-level transparent cache.** A protected deployment is no longer
   a Local Blossom Cache profile server.

## Rejected alternatives

| Alternative | Reason for rejection |
| --- | --- |
| **B: protect only upstream work on misses** (`ALMOND_UPSTREAM_FETCH_ACCESS`) | It protects fetch cost, not content. After one authorized fetch, a blob is publicly readable from the cache. The goal is a server whose blob reads are restricted as a whole. |
| **C: protect upstream-derived content** (`ALMOND_UPSTREAM_READ_ACCESS`) | It leaves native uploads public. A later upload of the same hash takes precedence over the cache entry and makes it public. Checking by source also needs source-aware gating inside the resolver instead of one gate. |
| `FEATURE_READ_AUTH=off\|public\|wot` naming | `off` would mean open, the opposite of other feature gates. `public` would silently become a whitelist whenever `ALMOND_ALLOWED_NPUBS` is set. |
| Signed-but-unrestricted access group | Anyone can generate keys, so it offers no protection against deliberate fetch load. |
| Always 401 for rejected reads | Clients could not tell a missing or invalid credential from a policy denial. |
| Redirects while protected | An authorized redirect hands out an upstream URL that works without Almond's authorization. |
| Admission-only authorization | Rejected in favor of enforcing expiry mid-stream. This deviates from the review recommendation and costs noticeably more to implement. |
| `private, no-cache` | Browsers would keep protected bytes on disk. `no-store` is the stricter choice. |

Only `ALMOND_READ_ACCESS` is part of the delivery. `ALMOND_UPSTREAM_FETCH_ACCESS` and
`ALMOND_UPSTREAM_READ_ACCESS` are not implemented.

## Out of scope

- Credentials for private upstreams. Almond never forwards the client's token,
  and an upstream 401 is not proof that a blob is absent.
- Per-owner or per-file ACLs.
- Quotas and rate limits. Authorization controls access; it does not limit
  volume.
- Tokens in query strings or cookies, or a media delivery bridge.
- Protection of other GET endpoints (`/list`, `/_upstream`, `/filter`,
  metrics, homepage, and so on).
- Encryption of stored blobs.

## Existing implementation caveats

These are pre-existing issues. This work does not change them.

1. `verify_event_with_policy` does not use `ALMOND_AUTH_CLOCK_SKEW`, so future
   events are rejected strictly. Reads inherit this. Do not document skew
   tolerance for reads.
2. `ALMOND_AUTH_MAX_TTL` must be greater than zero; `0` is a startup error
   (older docs wrongly described it as disabling the cap). Read tokens are
   therefore always capped.
3. Cashu download charging happens in `serve_blob`. Upstream response paths do
   not generally go through it. This work adds no new payment guarantee.
4. `AppError` serialization currently sends no `WWW-Authenticate` header. The
   read path must add the challenge as specified above.

## Acceptance criteria

### Configuration

- Without `ALMOND_READ_ACCESS`, or with `ALMOND_READ_ACCESS=public`, every blob request
  behaves as today. That covers status, body, and headers, including
  `public, max-age=31536000, immutable`.
- An unknown value fails startup. `whitelist` with an empty `ALMOND_ALLOWED_NPUBS`
  fails startup. `dvm` without `ALMOND_DVM_KINDS` fails startup.
- A non-public value with `ALMOND_UPSTREAM_MODE=redirect` or `redirect_and_cache`
  fails startup.
- `wot` starts the WoT refresh job, and `dvm` starts the DVM refresh job.

### Admission

- With `ALMOND_READ_ACCESS=whitelist`, all of the following hold for each source:
  upload, explicit mirror, native S3 (including unindexed objects), serve-files,
  upstream-cache hit, in-flight download, new upstream fetch, and known upstream
  miss.
  - Anonymous and invalid, expired, wrong-verb, wrong-`x`, and wrong-`server`
    tokens get 401 with `WWW-Authenticate: Nostr` and no blob bytes or metadata.
  - A valid token from a non-listed key gets 403.
  - A listed key is admitted.
- `wot` admits listed and trusted keys and returns 403 for others. `dvm` admits
  listed keys and announced DVMs and returns 403 for others.
- A valid token without `x` is admitted where the group permits. A token whose
  `x` or `server` tags include the requested hash or domain is admitted.
- HEAD, `Range`, `If-Range`, and `If-None-Match` (304) cannot bypass the gate.
  416 is only returned after authorization.
- A rejected request causes no outbound upstream request (HEAD or GET), no
  in-flight attachment, no background fill, no S3 lookup or publication, and no
  negative-cache change. Verify this by observing outbound traffic and state.
- Every follower of an in-flight download is authorized on its own.
- The incoming `Authorization` header is never sent to an upstream.

### Streaming expiry

- A protected GET whose token expires mid-body is aborted at the deadline, for
  a local file, native S3, an upstream proxy stream, and an in-flight follower.
  The client never receives a response that looks complete.
- Other readers of the same in-flight download continue. The background fill
  completes and publishes a hash-verified cache entry.
- A resumed request with a fresh token and `Range` continues correctly.

### Responses

- Every protected response (200, 206, 304, 416, HEAD, and all errors) carries
  `Cache-Control: private, no-store` and no `Expires`, including proxied
  upstream responses.
- 401 and 403 responses keep Blossom CORS headers.
- `WWW-Authenticate` is readable by browser clients.
- A CORS preflight succeeds without a token.
- Upload, mirror, chunk-upload, list, and delete status codes are unchanged.

### Verification scope

- Run a real server. For each caller type and each source, observe status,
  headers, body completeness, outbound upstream requests, and cache
  publication.
- Keep regression tests for token scope, the 401/403 mapping, startup
  validation, the gate-before-resolve invariant, and deadline aborts.
- Run the affected existing test modules in full.
- The earlier smoke run covered only anonymous local reads on today's binary. It
  is not evidence for any protected path.

## Decision register

| ID | Decision | Choice | Consequence |
| --- | --- | --- | --- |
| D1 | Protection scope | **A:** every blob GET/HEAD from any source; one gate before `resolve_blob` | All sources and upstream work are protected. Embeds need auth-capable clients. |
| D2 | Configuration | **`ALMOND_READ_ACCESS=public\|whitelist\|wot\|dvm`**, default `public` | Unknown values and `whitelist` without `ALMOND_ALLOWED_NPUBS` are startup errors. |
| D3 | Access groups | **anonymous, whitelist, WoT, DVM**; intended deployment `whitelist` | DVM relay load per unknown key is accepted and documented. There is no signed-unrestricted group. |
| D4 | Error responses | **401** (+ `WWW-Authenticate: Nostr`) for credential problems, **403** for policy denial | Applies to the read path only. Existing operations are unchanged. |
| D5 | Upstream delivery | **Proxy only** while protected | Redirect modes together with protection are a startup error. |
| D6 | Token expiry | **Enforced mid-stream** for client streams and followers; background fills continue | Deliberate deviation from the review recommendation. This is the largest implementation item. |
| D7 | HTTP caching | **`private, no-store`**, no `Expires`, for every protected response | Public paths keep today's headers. The internal blob store is unchanged. |

## Expected touchpoints

- [`src/config.rs`](../../src/config.rs), [`src/models.rs`](../../src/models.rs),
  [`src/main.rs`](../../src/main.rs):
  - strict `ALMOND_READ_ACCESS` parsing and startup validation (whitelist list, DVM
    kinds, redirect conflict);
  - `AppState` wiring;
  - WoT and DVM job conditions.
- [`src/services/authorization.rs`](../../src/services/authorization.rs),
  [`src/services/auth.rs`](../../src/services/auth.rs): `Operation::Read`,
  `validate_get_auth`, and the read-only 401/403 mapping.
- [`src/handlers/file_serving.rs`](../../src/handlers/file_serving.rs):
  - the gate before `resolve_blob`;
  - response finalization (cache headers, challenge);
  - the deadline body wrapper.
- [`src/error.rs`](../../src/error.rs),
  [`src/middleware.rs`](../../src/middleware.rs): the challenge on read 401s
  and `WWW-Authenticate` in the exposed CORS headers.
- [`src/handlers/upstream.rs`](../../src/handlers/upstream.rs): only where
  follower or proxy streams need to cooperate with the deadline wrapper or
  header finalization. No caller authentication is added there.
- `.env.example`, `README.md`, `CONTEXT.md`, and
  [`src/config-editor.html`](../../src/config-editor.html):
  - the `ALMOND_READ_ACCESS` contract and the startup rules;
  - the consequences above;
  - the CDN purge note for rollout.
