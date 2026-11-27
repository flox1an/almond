# Paid retention research: anonymous Cashu-paid Blossom storage

Date: 2026-10-30. This note is about feasibility only. It is not a decision and not an ADR.

Each claim is marked with one of these labels:

- **[SPEC]**: what a published spec says. The link points to the quoted text.
- **[PROPOSAL]**: an open upstream issue or PR. It is not normative.
- **[CDK]**: what the Cashu Development Kit source does at the linked tag.
- **[INFERENCE]**: my reading of the sources. No spec states it directly.
- **[RECOMMENDATION]**: a proposed direction for Almond, not an implemented feature.

Repository evidence, runtime observations and the proposed lifecycle follow in sections 8–10.

## Source snapshot

| Source | Revision read | Last change to the files read |
| --- | --- | --- |
| Blossom BUDs | [`hzrd149/blossom@b5bd280`](https://github.com/hzrd149/blossom/tree/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds) (master, 2026-06-15) | BUD-01 Sunset merged 2026-04-22 ([PR #99](https://github.com/hzrd149/blossom/pull/99)); BUD-02/06/07 last touched 2026-04-16 |
| Cashu NUTs | [`cashubtc/nuts@1363de4`](https://github.com/cashubtc/nuts/tree/1363de413294e2ef3a57b16950f9506af38fffc2) (main, 2026-10-23) | NUT-24 allowed `creqB` on 2026-06-23; NUT-18 payment-request consolidation landed 2026-07-21 ([#381](https://github.com/cashubtc/nuts/pull/381)) |
| CDK | `cdk 0.17.7` (the version pinned in `Cargo.lock`), compared against the latest release [`v0.18.3`](https://github.com/cashubtc/cdk/releases/tag/v0.18.3) (2026-10-29) | — |

Spec status as labeled in the files:

- [BUD-01](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/01.md) is `draft mandatory`.
- [BUD-02](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/02.md), [BUD-06](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/06.md), [BUD-07](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/07.md), [BUD-11](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/11.md) and [BUD-12](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/12.md) are `draft optional`.
- [NUT-03](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/03.md) is `mandatory`.
- NUT-07, 09, 12, 13, 18, 19, 24 and 26 are all `optional`. A mint may therefore lack state checks (NUT-07), restore (NUT-09), or response caching (NUT-19).

## 1. What Blossom defines about duration, renewal, receipts, ownership and payment identity

| Topic | What Blossom specifies | Gap |
| --- | --- | --- |
| Storage duration | **[SPEC]** BUD-01 only has an optional **`Sunset` response header** on `GET`/`HEAD /<sha256>`. It is "advisory only" and is "not guarantee" of deletion at that time. After the sunset the server may return 404, 410 or a redirect. Clients "MUST NOT" treat it as a permanent negative cache, and the blob may reappear ([BUD-01 § Sunset](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/01.md#sunset-header-optional)). | There is no request-side duration, no committed minimum retention, and no `expiration` field in the blob descriptor ([BUD-02 descriptor](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/02.md#blob-descriptor) has only `url`, `sha256`, `size`, `type`, `uploaded`, plus "MAY include additional fields"). |
| Renewal / top-up | **[SPEC]** Nothing. | No endpoint, header or price semantics exist for extending a blob's retention. |
| Paid lease semantics | **[SPEC]** BUD-07 is per-request payment only. It lists the endpoints that "may" require payment (`PUT /upload`, `GET`, `HEAD`, `/media`) and says nothing about what the payment buys in time ([BUD-07](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/07.md#payment-required)). | Payment is not linked to a retention window. |
| Receipts | **[SPEC]** None. A successful `PUT /upload` returns a blob descriptor (201 if newly stored, 200 if it already existed) ([BUD-02](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/02.md#put-upload---upload-blob)). BUD-07 defines no payment-acknowledgement header. | No payment receipt, transaction id, or "paid until" field. |
| Ownership | **[SPEC]** Identity exists only through optional BUD-11 Nostr authorization (kind `24242`, `t` verb, `x` hash scope, required `expiration`) ([BUD-11](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/11.md)). BUD-12's optional, "unrecommended" `/list/<pubkey>` returns blobs "uploaded by the specified pubkey". `DELETE` "MUST" be accepted, but authorization is optional ([BUD-12](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/12.md)). | Anonymous (unauthenticated) uploads have no defined owner. Blobs are content-addressed and deduplicated (the 200 response on re-upload), so a blob may have zero, one or many "uploaders". |
| Payment ID / idempotency | **[SPEC]** BUD-07 tells the client to "retry the request using the same `X-{payment_method}` header" with a proof. A failed proof "MUST" get **400** plus `X-Reason` ([BUD-07 § Client implementation / Error handling](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/07.md#client-implementation)). `HEAD` "MUST NOT be retried with payment proof". | No payment ID and no idempotency key. The spec says nothing about a retry whose payment already settled. |
| Pre-flight | **[SPEC]** `HEAD /upload` may return 402, but "the result is not a guarantee of the eventual `PUT /upload` outcome" ([BUD-06](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/06.md)). | A quote from `HEAD` does not bind the later `PUT`. |

**[INFERENCE]** Blossom defines a paywall in front of HTTP operations and a way to announce expected deletion. It does not define paid storage time, renewal, receipts, ownership of anonymous blobs, or payment idempotency. Any paid-retention behavior in Almond is a server extension until upstream specifies it.

### Open upstream proposals (all non-normative)

- **[PROPOSAL]** [blossom#111 "Paid retention"](https://github.com/hzrd149/blossom/issues/111) (open, 2026-08-25, 0 comments). It proposes:
  - a client-requested window `X-Expiration: <unix-ts>`, on upload or on a dedicated `PUT /retain/<sha256>`;
  - the server prices size × duration and answers with a BUD-07 402;
  - after payment the server "commits to retain the blob until T" and reflects T via `Sunset`;
  - a top-up repeats the flow with a later T and the server "prices only the delta";
  - a paid lease acts as a retain-until floor;
  - the issue states that retention "is an availability commitment, not a cryptographic proof".
- **[PROPOSAL]** [blossom#115 "Client-requested blob expiration"](https://github.com/hzrd149/blossom/issues/115) (open, 2026-10-05):
  - `X-Expiration` on `PUT /upload` / `PUT /mirror`;
  - the expiration belongs to the uploader pubkey, and the effective blob expiration is the latest of all owners' expirations;
  - a later time extends it; whether an earlier time can shorten it is left to the server;
  - `Sunset` echoes the effective value;
  - it is explicitly "a hint … isn't a guarantee or a quota mechanism".
- **[PROPOSAL]** [blossom#112 "Anonymous per-operation payment session"](https://github.com/hzrd149/blossom/issues/112) (open, 2026-08-26) proposes a signed quote plus a bearer session token bound to a declared hash set. Among its open problems it lists: "if the client pays but the session response is lost, it must be able to recover the same token via a client chosen nonce or a proof it can present again. This is critical for Cashu, where the token is already redeemed."
- **[PROPOSAL]** [blossom#76 "account based quotas"](https://github.com/hzrd149/blossom/pull/76) (open PR, 2025-07-01) documents nostr.download's account/subscription payments API. In the PR conversation, hzrd149 wrote that such plans "should be handled on a per server basis" (comment dated 2025-07-30).
- **Closed:** [blossom#63](https://github.com/hzrd149/blossom/issues/63). It discussed `X-Expiration`, top-up-to-extend, and how expiration interacts with payment. It was closed on 2026-04-22 with a pointer to the `Sunset` header only. The payment question was left unanswered, which #111 also notes.

## 2. What NUT-24 guarantees and what it does not

[NUT-24](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/24.md) depends on NUT-18 and NUT-26.

**[SPEC] What NUT-24 does guarantee:**

- The server's 402 carries `X-Cashu` with a payment request encoded as `creqA` (NUT-18 CBOR) or `creqB` (NUT-26 Bech32m). Wallets "MUST" support both formats.
- The decoded request fields are `a` (amount), `u` (unit), `m` (mints) and an optional `nut10` lock. There is "no transport field since we expect an in-band payment".
- The client retries with a `cashuB` token in `X-Cashu`. The token "MUST be from one of the mints listed in the `m` array", of unit `u`, and "greater than or equal to the `a` amount".
- A wrong mint, wrong unit, insufficient amount or insufficient lock "should" get HTTP 400.

**[SPEC / INFERENCE] What NUT-24 does not provide:**

- **No payment ID binding.** NUT-18 returns `i` in its transport's `PaymentRequestPayload` ([NUT-18](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/18.md#payment-payload)); NUT-24 uses a bare `cashuB` without obligatory payment ID. **[INFERENCE]** Bind payment to an application operation. Canonical mint-scoped proof identities (`Y`) offer a replay key; serialized token hashes do not, since encoding and ordering vary. An application operation ID is useful but not supplied automatically by NUT-24.
- **No receipt, no change, no refund.** Nothing defines a response header for a successful payment, for change on overpayment, or for refunds.
- **No settlement or atomicity semantics.** NUT-24 does not say when the server must redeem the token, or what happens if the operation fails after redemption.
- **No quote expiry or price pinning.** A NUT-18 request has no expiry field.
- **Fee semantics are ambiguous.** NUT-24 says the token must be ≥ `a`. NUT-18, on which NUT-24 depends, now defines `a` as "net of input fees": `sum(proofs) - input_fee(proofs) >= a + mf` ([NUT-18 § Input fees](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/18.md#input-fees)). The receiver pays the input fee when it swaps ([NUT-02 § Fees](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/02.md#fees)). **[INFERENCE]** On a mint with nonzero `input_fee_ppk`:
  - a wallet that follows NUT-18 will overpay the gross amount;
  - a wallet that follows NUT-24's literal "≥ `a`" may underpay the net amount;
  - a server that checks the gross token value before the swap and the net amount after the swap can accept a token pre-swap, redeem it, and then find it short.
- **Payment-request discoverability and compliance.** BUD-07 requires `X-Cashu` to adhere to NUT-24 ([BUD-07 § X-Cashu](https://github.com/hzrd149/blossom/blob/b5bd2801d1763aa635fc8fea7a76597e0eb18990/buds/07.md#x-cashu-header)). Almond's existing noncompliance (it emits a JSON `cashuA` challenge instead of `creqA`/`creqB`) is already tracked in [`compliance.md`](compliance.md).

## 3. Settlement mechanics: what "paid" means in Cashu

### Protocol

- **[SPEC] The swap is the settlement.** In "Swap to receive", the receiver's swap invalidates the sender's proofs. "Only if `Carol` has redeemed new outputs, `Alice` can't double-spend the `Proofs` anymore and the transaction is settled" ([NUT-03](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/03.md#swap-to-receive)).
- **[SPEC] A DLEQ check alone is not settlement.** NUT-12 lets a receiver verify the mint's signature offline ([NUT-12](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/12.md)), but it does not show that the proofs are unspent. **[INFERENCE]** Accepting a payment without a swap requires local state: you must remember which Ys you have accepted.
- **[SPEC] A validated NUT-11 P2PK lock can exclude the sender.** Additional keys, signature thresholds, locktime and refund paths matter ([NUT-11](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/11.md)). **[INFERENCE]** It does not stop identical unredeemed proofs being submitted for multiple purchases. NUT-07 `UNSPENT` does not reserve them: use a swap or durable atomic accepted-proof registry shared by all accepting instances.
- **[SPEC] Tools for resolving ambiguous mint outcomes:**
  - **NUT-07** `POST /v1/checkstate` returns `UNSPENT`/`PENDING`/`SPENT` per Y ([NUT-07](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/07.md)).
  - **NUT-19** lets mints cache successful responses (for example `POST /v1/swap`) keyed by method, path and payload, for an advertised `ttl`. Replaying the *identical* request returns the cached signatures ([NUT-19](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/19.md)).
  - **NUT-09** `POST /v1/restore` reissues signatures for previously signed `BlindedMessage`s, "for recovering the response of an interrupted swap request" ([NUT-09](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/09.md)). With **NUT-13** deterministic secrets, the receiver can regenerate those blinded messages from its seed and counter ([NUT-13](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/13.md)).
  - All of these are optional. A given mint must advertise them in `/v1/info` (NUT-06).

### CDK receive behavior (0.17.7, the version Almond pins)

1. **[CDK] Single-mint, single-unit wallet.** `Wallet::receive` rejects a token whose unit differs from the wallet's or whose mint is not the wallet's mint (`IncorrectMint`) ([receive/mod.rs#L61-L90](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/mod.rs#L61-L90)).
2. **[CDK] Receive is a write-ahead saga persisted in the wallet database** ([receive/saga/mod.rs#L252-L461](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/saga/mod.rs#L252-L461)):
   - it stores the input proofs as `Pending` and creates a saga in `ProofsPending`;
   - it builds the swap;
   - it records `counter_start`/`counter_end` and the blinded messages, then moves the saga to `SwapRequested` before calling the mint;
   - after `post_swap` it stores the new proofs, adds a `Transaction`, and deletes the saga.
3. **[CDK] Timeouts leave the outcome ambiguous.** If `post_swap` fails, CDK runs compensation only when `err.is_definitive_failure()` is true ([error.rs#L672](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk-common/src/error.rs#L672)). Otherwise it logs "ambiguous", leaves the saga in `SwapRequested`, and returns `Err` (lines L375–L386 of the saga file above). **[INFERENCE]** For a network timeout the caller cannot tell whether the mint swapped. The server cannot tell whether it was paid.
4. **[CDK] Recovery happens only when the application asks for it.** `Wallet::recover_incomplete_sagas()` is documented as "Call on startup" and "Requires network connection" ([recovery.rs#L296-L302](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/recovery.rs#L296-L302)). The crate README marks it "Required" ([wallet/README.md](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/README.md)). For a `SwapRequested` receive it:
   - replays the stored swap request, so NUT-19 can help;
   - if that fails, checks NUT-07 state;
   - if the inputs are spent, restores outputs via NUT-09;
   - if they are unspent, compensates;
   - if the mint is unreachable, skips and retries on a later run.

   See [receive/saga/resume.rs#L77-L175](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/saga/resume.rs#L77-L175).
5. **[CDK] Recovery fixes funds, not records.** In 0.17.7 the recovery path updates proofs but does **not** add a `Transaction` and does not report which token or blob the payment was for (`complete_receive_from_restore`, same file). **[INFERENCE]** Recovery protects the operator's funds. It does not tell the application that "payment X for blob H succeeded".
6. **[CDK] What a transaction record contains.** A record has `ys`, `amount` (net), `fee`, `timestamp` and `metadata: HashMap<String,String>`. In 0.17.7 its ID is `sha256(sorted Ys)` ([cdk-common wallet/mod.rs#L527-L565, L643-L654](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk-common/src/wallet/mod.rs#L527-L565)). `ReceiveOptions.metadata` is copied into the transaction ([#L467-L476](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk-common/src/wallet/mod.rs#L467-L476)). `Wallet::get_transaction(id)` and `list_transactions` exist ([transactions.rs](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/transactions.rs)).
7. **[CDK] Change in v0.18.3.** The receive saga now upserts a `Pending` transaction, including the caller's `metadata`, *before* `post_swap`. It marks the transaction failed on a definitive error and completes it on recovery ([v0.18.3 receive/saga/mod.rs](https://github.com/cashubtc/cdk/blob/v0.18.3/crates/cdk/src/wallet/receive/saga/mod.rs), [v0.18.3 receive/saga/resume.rs](https://github.com/cashubtc/cdk/blob/v0.18.3/crates/cdk/src/wallet/receive/saga/resume.rs)). Recovery uses `TransactionId::from_saga_id`.

   **[INFERENCE]** `ReceiveOptions.metadata` is documented as a `HashMap` ([docs.rs cdk 0.17.0 ReceiveOptions](https://docs.rs/cdk/0.17.0/cdk/wallet/struct.ReceiveOptions.html)). It is a candidate for tagging a payment with application context (for example `blob=<sha256>`, `until=<ts>`). Even so:
   - In 0.17.7, recovery after a timeout does not write a transaction or metadata. Only 0.18.x carries metadata through an ambiguous swap.
   - End-to-end recovery through metadata is **not proven**. It needs an implementation spike.
   - The wallet SQLite and blob storage are separate stores, so writing a wallet record and committing the blob is **not atomic**.

## 4. Anonymous bearer payments vs receipts and recovery

**[INFERENCE] Constraints that follow from the sources above:**

- **Every uncertain case needs state somewhere.** The mint's spent set is the global anti-replay state. Each receiver also needs local state: CDK's wallet SQLite already exists and is a database, with proofs, transactions and sagas. "No application database" can only mean "no *additional* application database". It cannot mean "stateless".
- **Spent tokens are not self-verifying service receipts.** Their identities can locate an original operation, but do not prove who redeemed them or which blob was credited. Never treat `SPENT` alone as proof of receipt by Almond.
- **Sponsorship needs no payer ownership.** Anyone can extend a public hash's retention under this proposed model. Payment grants no deletion, shortening or refund rights. BUD-11 proves authorization by a key, not ownership of a blob; owner-only deletion needs an additional ownership/capability policy.
- **No stateless atomic payment-plus-storage commit.** A durable operation journal can make retries and application of an entitlement idempotent while recovery resolves uncertain settlement. Do not blindly charge fresh proofs after a timeout or promise a distributed SQL transaction.

### Failure cases for one paid upload or extension (upload ordering: stream/hash → swap → commit)

| Failure point | Who holds the money | What the client sees | What recovers it |
| --- | --- | --- | --- |
| Token rejected before swap (mint, unit, amount, malformed) | Payer | 400 (BUD-07/NUT-24) | Nothing to recover |
| Mint definitively rejects the swap (e.g. already spent) | Payer, or whoever spent it | 400 | Nothing to recover |
| Swap times out (ambiguous) | Unknown | Error | Resolve using recorded outputs and wallet recovery; `SPENT` alone does not prove receipt. Keep the application operation unresolved meanwhile. |
| Swap succeeds, blob commit fails or server crashes | Operator | Error/drop | Durable operation linkage and recoverable staged bytes permit completion; otherwise a defined refund/reupload recovery path is needed. |
| Swap and commit succeed, response is lost | Operator | Timeout | Replay the recorded operation result. HEAD/Sunset shows availability, not which purchase succeeded. |

**[RECOMMENDATION] Minimal reliable direction:**

1. **Expose entitlement, not proof of payment.** Return service-defined `paid_until` and expected expiry through `Sunset`. BUD-01's header remains advisory; the stronger promise comes from the service contract. Neither replaces a journal.
2. **Stage durably, record intent, settle, publish.** Preserve verified bytes and immutable hash/entitlement/payment terms before contacting the mint. Apply the recorded purchase idempotently afterward. Merely charging after public publication exposes unpaid data.
3. **Bind retries to the same purchase.** Match canonical proof identities and/or application operation ID to immutable original terms. Return a completed result without swapping or extending again. CDK metadata alone is insufficient for 0.17.7 timeout recovery.
4. **Call `recover_incomplete_sagas()` at startup and periodically.** The version-pinned docs require it after wallet creation ([docs.rs cdk 0.17.0 `Wallet`](https://docs.rs/cdk/0.17.0/cdk/wallet/struct.Wallet.html); [crate README](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/README.md)). Per the parent's repository check, Almond's `init_wallet` only calls `CdkWallet::new` and makes no recovery call. That is an integration gap specific to this version. It is not demonstrated fund loss: the stuck sagas persist in the wallet database and could be recovered later.

## 4a. Why the local Cashu SQLite exists, and the alternatives that forward payments

**[CDK] What the wallet database holds in 0.17.7.**

- **Received proofs.** These are the operator's revenue. Cashu proofs are bearer secrets: losing them loses the money.
- **In-flight receive sagas.** These include the pending input proofs, the outgoing blinded messages, and the NUT-13 counter range, all written *before* the mint call (§3, item 2).
- **Per-keyset derivation counters.** NUT-13 says the wallet "MUST keep track of" a counter per keyset ([NUT-13](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/13.md#counter)).
- **Transactions.** See §3, item 6.

**[SPEC/CDK] The seed is a partial backup.** With the seed, a wallet can regenerate its blinded messages and ask the mint to reissue the signatures ([NUT-13 § Restore](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/13.md#restore-from-seed-phrase), NUT-09; CDK `Wallet::restore()`). This only works if the mint supports NUT-09. Losing the database also loses in-flight saga context and payment-to-blob correlation.

**[INFERENCE] Can the local SQLite be removed by forwarding every payment?** The options differ in where the durable state ends up. None of them removes the need to durably record "this blob was credited for this payment".

| Option | Mechanism | Risk / what still needs durable state |
| --- | --- | --- |
| A. Forward the raw incoming token unswapped | Pass the payer's `cashuB` string to a central wallet without swapping | **[SPEC]** Settlement happens only when the receiver swaps ([NUT-03 § Swap to receive](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/03.md#swap-to-receive)). Until the central wallet swaps, the payer can double-spend the proofs. Granting retention before then gives storage away on credit. |
| B. Swap locally, then export to the central wallet | Receive into the local wallet (settles), then create a send token for the central wallet | The local wallet still holds funds between the swap and the export. It needs crash-durable state for the receive saga *and* the outgoing send: the token is reserved or pending until the central wallet claims it. CDK has send sagas and reclaim for this ([`send/`](https://github.com/cashubtc/cdk/tree/v0.17.7/crates/cdk/src/wallet/send), [`reclaim.rs`](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/reclaim.rs)). This shrinks the stored balance but keeps the database. |
| C. External wallet service does the swap | Almond calls a service (its own CDK, a mint-side wallet, etc.) that swaps and returns a durable acknowledgement with an idempotent status lookup | This moves persistence to the service; it does not remove it. Almond must still handle an ambiguous call (timeout) by querying status with a stable key, such as the token's proof Ys. **An acknowledgement or a delivered token is not proof of redemption** unless the service reports that it completed the swap. |
| D. P2PK-lock payments to a central key | The 402 request sets `nut10` to a P2PK lock ([NUT-24](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/24.md#server-response), [NUT-11](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/11.md)). Almond holds no private key and forwards the locked proofs. | See the list below this table. |

**Why option D still needs durable state:**

1. **[SPEC]** A lock may have a `locktime` and `refund` keys. The payee "must validate the incoming tokens themselves", including "the DLEQ proof and whether the token includes a long-enough timelock" ([NUT-18 § Locking conditions](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/18.md#locking-conditions)).
2. **[SPEC]** Without the private key, Almond cannot swap. The best it can do is a NUT-07 check, and `UNSPENT` means only that "the mint has no record of it being pending or spent" ([NUT-07](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/07.md#token-states)). That check does not show the proof is genuine; only a NUT-12 DLEQ check does that offline ([NUT-12](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/12.md)).
3. **[INFERENCE]** Even an exclusive lock permits replaying identical proofs for several purchases. Atomically register accepted Ys across all instances; retain operation linkage for retries after redemption too.
4. **[INFERENCE]** The proofs themselves must remain durably available to the central wallet. Never discard the only copy after verification. Any sender refund deadline bounds redemption time.

**[INFERENCE] Bottom line.** Forwarding can remove the local *funds* store and the local private keys (option D) or shrink them (option B). It cannot remove the need to durably record which payment credited which blob. The minimum durable state is roughly "accepted proof Ys → blob sha256 → paid-until", plus whatever in-flight state the chosen settlement path needs.

## 4b. Must received or stored tokens be P2PK-locked?

**[SPEC] No.** P2PK is not mandatory.

- BUD-07 does not mention locking.
- In NUT-24, `nut10` is optional: "The required NUT-10 locking condition" is listed with `<optional>` ([NUT-24](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/24.md#server-response)).
- NUT-11 itself is `optional` ([NUT-11](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/11.md)).
- A 402 without `nut10` asks for ordinary bearer ecash, and an immediate swap settles it (NUT-03).

**[CDK] What happens to proofs Almond stores.**

- The receive saga swaps the inputs for outputs with `spending_conditions = None` ([receive/saga/mod.rs#L322-L337](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/saga/mod.rs#L322-L337); [`create_swap` signature](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/swap/mod.rs#L122-L135)).
- So the stored proofs are fresh, seed-derived (NUT-13) proofs. They do **not** inherit any lock the incoming token had.
- Active recipient-only P2PK locks require a matching signature. CDK can use supplied or stored signing keys, or a sufficient existing witness ([receive preparation](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/saga/mod.rs#L127-L227)). Expired locks and alternative spending paths require separate treatment; a P2PK secret alone does not imply exclusivity.

**[INFERENCE] Supporting both locked and unlocked payments means two separate validation and redemption paths:**

| Payment lock | Who can redeem | What the server must do |
| --- | --- | --- |
| Unlocked (bearer) | Whoever swaps first | Swap immediately. That swap is both the validation and the settlement. |
| Locked to a **server** key | The server, if a signing key is injected into its wallet | Hold a private key, which is the same custody as unlocked. The benefit is that an intercepted token cannot be stolen before the swap. |
| Locked exclusively to a **central wallet** key | Only that wallet | Forward for confirmed redemption, or explicitly implement deferred acceptance with durable proofs, validation and shared replay protection. |

**What offline validation must cover for a locked payment:**

- the mint advertises NUT-11 support;
- a DLEQ is present and valid (NUT-12);
- amount and unit are correct;
- the lock `data` is the expected key;
- every spending path and threshold enforces the intended recipient policy; reject unexpected additional keys rather than assuming `n_sigs = 1` excludes other signers;
- `locktime`/`refund` are absent or comfortably beyond redemption, because after `locktime` the refund keys can spend ([NUT-11 § Tags/Locktime](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/11.md#locktime-tag));
- `SIG_ALL` constraints, if present, are understood;
- the proofs have not been accepted before (a durable record of Ys).

NUT-18 states this responsibility directly: the payee "must validate the incoming tokens themselves" ([NUT-18 § Locking conditions](https://github.com/cashubtc/nuts/blob/1363de413294e2ef3a57b16950f9506af38fffc2/18.md#locking-conditions)).

**[RECOMMENDATION]** For the standalone host, default to unlocked payments with an online swap. Add P2PK locking only when external custody or offline receive is actually needed.

**Current Almond:** The challenge requests no lock; default receive options supply no application signing keys. No receiving P2PK key is generated or advertised (`src/error.rs:88–101`, `src/services/cashu.rs:400–404`). Ordinary operation is free tokens plus online swap, not an explicit rejection of every syntactically locked proof.

**Both are possible:** A quote requiring a lock must not silently accept unlocked tokens instead. A no-lock quote may accept additional redeemable tokens, but not arbitrary locks just because amount/mint match. `cashuA` versus `cashuB` means serialization, not locking. No live P2PK settlement was exercised.

## 5. Overpayment, change, and pasted tokens

- **[SPEC]** NUT-24 accepts "greater than or equal to the `a` amount". No mechanism returns change in-band. NUT-18 single-use (`s`) and `i` are not carried by NUT-24.
- **[CDK]** `receive` swaps *all* proofs in the token and returns the net amount. There is no partial-receive or return-change API on the receive path (receive/saga, above).
- **[INFERENCE]** A NUT-24-aware wallet decodes `creqA`/`creqB` and is expected to build a token of exactly the right amount (NUT-18 tells the payer to account for input fees). A user who *copy-pastes an existing token*, for example a 1 000-sat token for a 10-sat upload, will overpay. The server keeps the whole amount unless it adds a non-standard mechanism.
- **[RECOMMENDATION] Overpayment handling, cheapest first:**
  - (a) Before the swap, reject tokens whose gross value exceeds `quote + fee allowance + tolerance`. Return 400 with an `X-Reason` that tells the user to send an exact token. No funds move.
  - (b) Offer surplus as more retention only with user approval. Reject excess beyond the maximum creditable horizon before swapping.
  - (c) Non-standard change as `cashuB` needs durable outgoing state and safe retrieval after response loss; a one-shot header is insufficient.
  - Whichever is chosen, publish the fee interpretation: `a` should be net of input fees, per NUT-18.

## 6. Scope note: operator product

The target is an almost-zero-config public Blossom host selling storage time for Cashu. Friends-only quotas are a separate future product, not a prerequisite.

- **[INFERENCE]** Nothing in BUD-01/02/06/07 or NUT-24 requires accounts, per-user quotas or multi-tenancy to sell storage time. Payment is per request and bearer. The blob hash is the unit being paid for. `Sunset` is the standard place to display what was bought. Proposals #111 and #115 also attach the retention state to the blob (and to the uploader pubkey only in #115, which is the free-hint variant). Account quotas (#76) are a separate product direction, and the maintainer considers them server-specific.
- **[RECOMMENDATION] Product choices that must exist, but need not all become settings:**

  | Setting | Why it is needed |
  | --- | --- |
  | Accepted mint (CDK wallet is bound to one mint URL and unit) | Single-mint, single-unit wallet behavior in §3 |
  | Price per size × time | Needed to price storage time at all |
  | Maximum or default retention horizon | Bounds what one payment can buy |
  | Revenue custody | The wallet seed and database are the funds. Backup/withdraw (melt) is the operator's responsibility. |
  | Storage durability backend | Determines whether paid-until is honored. #111 notes retention "is an availability commitment, not a cryptographic proof". |

  Everything else can have defaults. Presets (for example "public paid host") are a better way to cover the remaining knobs than exposing each one.

## 7. Spec gaps (summary)

1. **Blossom:** no standard request for a paid retention window, no renewal, no paid-until field in the descriptor (`Sunset` is advisory only), no receipt, no payment ID or idempotency, and no defined owner for anonymous blobs. Proposals #111, #112 and #115 are open with 0 comments as of 2026-10-30.
2. **NUT-24:** no request/payment binding, no change or refund, no quote expiry, no settlement timing, and a fee definition (gross vs net) that conflicts with NUT-18's net-of-fees `a`.
3. **Cashu recovery:** depends on optional mint features (NUT-07/09/19). It recovers funds, not the business context. In CDK 0.17.7, a payment recovered after a timeout leaves no transaction record or application metadata. CDK 0.18.3 adds a pending-transaction journal that carries metadata.
4. **Exactly-once across mint and storage** is not provided here. Use recoverable settlement and idempotent application of recorded purchases, not a claimed atomic transaction spanning both.

## 8. Current Almond evidence

Repository observations, not upstream requirements:

| Area | Current behavior | Evidence |
| --- | --- | --- |
| Wallet | SQLite plus separate 64-byte seed; initialized for paid operations | `src/services/cashu.rs:317–378`, `src/lib.rs:428–454` |
| Payment context | `charge` returns `()`; receives operation type and size, not hash or retention. Default receive options supply no application metadata | `src/services/cashu.rs:88–151,400–404` |
| Upload ordering | Stream/hash and authorize, charge, then finalize storage | `src/handlers/upload.rs:28–84` |
| Deduplication | Existing Upload-origin hash retains previous metadata; reupload is not renewal | `src/services/file_storage.rs:157–176` |
| Local metadata | Expiration in filename; age from mtime; pubkey reconstructed as `None` | `src/utils.rs:224–272` |
| S3 metadata | Expiration in object key; age from LastModified; pubkey reconstructed as `None` | `src/services/native_storage.rs:373–427` |
| Retention | Earlier of client expiration and configured max age | `src/utils.rs:368–405` |
| Capacity | Cache first, then oldest uploads; no paid-retention protection | `src/utils.rs:308–352` |
| Admission | Counts duplicate as new bytes/blob; no durable concurrent capacity reservation | `src/services/file_storage.rs:586–612` |
| Pending bytes | Startup clears `temp/`; not recoverable paid staging | `src/lib.rs:290–318,368–381` |
| Wallet recovery | No call to `recover_incomplete_sagas` during initialization | `src/services/cashu.rs:367–378` |
| API | No dedicated renewal, payment-status or revenue withdrawal route | `src/lib.rs:233–269`; receive-only integration in `src/services/cashu.rs` |
| Identity | Public writes require valid Nostr auth; stored blob metadata has no uploader key | `src/handlers/upload.rs:28–32`, `src/services/file_storage.rs:34–46` |

**[RECOMMENDATION]** `paid_until` is a minimum preservation commitment, not the existing expiration hint. Ordinary cleanup, smaller future configuration limits and FIFO must not delete paid blobs early. Reserve capacity under concurrency and reject purchases before charging when they cannot be honored. Moderation/legal removal and hardware failure need explicit service policies, not impossible durability promises.

## 9. Proposed minimal service lifecycle

This is a product proposal, not implemented endpoints or a finalized schema.

### Customer purchase and renewal

1. **Quote:** Bind hash, size, absolute retention target, net price, mint and validity. One default duration and one size/time tariff suffice. Optional HEAD preflight never takes money.
2. **Stage:** Verify bytes, check quarantine, reserve capacity. Preserve staged bytes across restart once payment may start. Existing bytes need no reupload for renewal.
3. **Record intent:** Persist immutable terms, payment association and wallet recovery linkage before the external mint call. Never log bearer tokens.
4. **Settle:** Redeem locally or obtain authenticated durable settlement confirmation externally. Timeouts remain unresolved, not an invitation to charge again.
5. **Apply once:** Publish bytes and persist the purchased entitlement; mark the operation applied. Reconcile wallet/filesystem/S3/app state rather than assuming cross-store atomicity.
6. **Return/recover:** Return URL, `paid_until` and an accountless operation reference. Repeating a request returns the same result, not another duration increment.

Minimum logical durable state; physical tables/files remain an implementation choice:

| State | Required information | Not required |
| --- | --- | --- |
| Blob entitlement | Hash, verified size, location, paid-until, availability/moderation state | Payer name, account, uploader identity |
| Purchase journal | Operation ID, immutable terms, canonical mint-scoped proof identities, settlement linkage/outcome, application result | Customer profile or full activity history |
| Wallet | Spendable proofs, derivation/recovery state, in-flight operations, locally or externally | Separate database server |
| Withdrawal | Operator-authorized amount/target, request ID, reserved funds, outgoing token or melt quote, recovery/status | Customer ownership records |

**Renewal example:** A hash paid through 1 December can be sponsored by anyone through 1 January; price only the additional interval. Applying that purchase uses `max(current_paid_until, purchased_target)` so replay cannot extend it twice. Concurrent sponsors need serialized quoting/commit or an explicit overlap/refund policy: `max` alone does not prevent overlapping charges. If bytes were deleted, reject before taking money or require reupload. If expired but present, price new coverage from now, not an elapsed gap.

**Receipts and privacy:** A customer-held receipt can be optional, but internal recovery state is not. A signed receipt proves a promise, not ongoing storage. Payment grants no deletion rights. Fresh browser-generated Nostr keys satisfy the existing public-write path without login, but IP/timing, content and mint interactions can correlate activity; do not promise network anonymity ([Cashu FAQ](https://docs.cashu.space/faq)).

### Operator withdrawal is part of the initial product

**[RECOMMENDATION]** A host that collects revenue without a usable withdrawal path is incomplete. Prefer one first-class Cashu export path over silently accumulating an inaccessible wallet. The public frontend can include a separate protected operator page; it must not expose wallet operations through public BUD payment routes.

1. **Bootstrap operator access locally.** Generate a high-entropy admin credential on first start, store it with restrictive permissions, and deliver it through a local-only mechanism. Do not allow first-public-visitor ownership claiming. A separate admin credential can reuse the repository's bearer-auth convention (`src/handlers/metrics.rs:10–22`) without reusing the metrics secret itself. Owner-pubkey authentication is an alternative, not a requirement for payment collection.
2. **Show spendable, reserved and pending funds**, mint and unit; do not present all wallet proofs as spendable balance. Show fees before confirmation.
3. **Prepare and confirm a withdrawal under an idempotent request ID.** Atomically reserve selected funds so parallel withdrawals cannot spend them twice. CDK 0.17.7 provides `prepare_send`, `PreparedSend::confirm/cancel`, fee inspection and an operation ID ([PreparedSend docs](https://docs.rs/cdk/0.17.7/cdk/wallet/struct.PreparedSend.html), [send source](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/send/mod.rs)).
4. **Return a Cashu token to copy or scan into the owner's wallet.** Persist the outgoing operation and allow the same authorized owner to retrieve the same token after a lost response; retry must not create a second withdrawal. CDK tracks `TokenCreated` sends with proof data and provides `get_pending_sends`, `check_send_status` and `revoke_send` (same source). Exact HTTP-to-CDK crash recovery integration still needs implementation verification.
5. **Distinguish exported from redeemed.** Mark the token pending until the destination swaps it. Offer explicit reclaim of an unredeemed export through a swap, not simply marking its proofs spendable again. Once claimed, reclaim must fail or reconcile to claimed. A paste into another wallet is not by itself confirmation of receipt.

**Lightning alternative:** The owner pastes a BOLT11 invoice; obtain a melt quote, display amount plus fee reserve, require confirmation, and persist the quote/operation before execution. Resume status by the same quote after a timeout, never a fresh payment. NUT-05 defines `UNPAID`/`PENDING`/`PAID`, quote expiry and status lookup; NUT-23 specifies BOLT11 and NUT-08 fee change ([NUT-05](https://github.com/cashubtc/nuts/blob/main/05.md), [NUT-23](https://github.com/cashubtc/nuts/blob/main/23.md)). This is a valid first payout choice instead of Cashu export, not a prerequisite to implement both. It adds Lightning fee, liquidity and pending-payment behavior.

**Admin security:** HTTPS or a local tunnel, same-origin restricted operator UI, authentication on every balance/export/status/reclaim call, CSRF defenses appropriate to session choice, no bearer secrets in URLs/logs, and `Cache-Control: no-store` on sensitive responses. CORS is not authentication. Existing Nostr kind-24242 blob permissions are not a withdrawal authorization scheme. If using signed owner requests, bind method, server, amount, destination, expiry and a one-use nonce; a generic login signature must not authorize arbitrary payouts.

### Owner-locked revenue: optional later hardening

**[INFERENCE / RECOMMENDATION]** The suggested owner public key is a treasury spending key, not the customer identity or blob owner. Minting new proofs exclusively locked to that key can protect already-locked revenue from a server disk leak. The private key stays off the host. Possession of the public key alone cannot withdraw; the owner must authorize spending using the matching private key. NUT-11 enforces signatures on proofs (or aggregated inputs/outputs for `SIG_ALL`), not a generic website login ([NUT-11](https://github.com/cashubtc/nuts/blob/main/11.md)). A Nostr extension login does not automatically imply support for the required Cashu signing format.

Implementation routes differ: require incoming tokens locked to the treasury key and forward/store them with shared replay protection; directly swap free inputs into owner-locked outputs; or receive normally and re-lock with a second swap. The last route leaves an unlocked window and may incur another fee. Current CDK 0.17.7 `receive` explicitly builds outputs without spending conditions, so direct owner-locking is not an existing Almond configuration or `ReceiveOptions` switch ([receive source](https://github.com/cashubtc/cdk/blob/v0.17.7/crates/cdk/src/wallet/receive/saga/mod.rs#L329-L342)). Customizing it needs crash-recovery compatibility, not a second hand-written wallet protocol.

This does **not** make server compromise harmless: the attacker can delete proofs/data, change future output targets or payment challenges, and compromise a server-delivered operator page to redirect signing requests. Owner verification must bind exact withdrawal outputs; `SIG_INPUTS` alone is not transaction-output authorization. Mint compromise remains a separate trust risk. Locked proofs still need durable backup and delivery; an owner key cannot reconstruct random proof data by itself. Defer this option as requested, but ship the ordinary authenticated withdrawal path in the initial paid-host product.

### Deployment and refactoring target

Keep embedded SQLite and auto-create wallet/seed/application state under one persistent data directory. No PostgreSQL, DB administrator or second service is needed. A file journal could replace SQL, but reimplements durable transitions and locking. Mint choice is a custody/trust decision; tariff, duration and capacity policy can have documented defaults. Public origin/TLS and backups remain operational requirements, not reasons for dozens of settings.

External wallet custody adds endpoint/auth/availability/recovery dependencies. Useful for a central treasury across servers, not the simpler standalone default. Never share copied wallet seeds/databases across independent instances without coordination of counters and pending proofs.

Reuse `cashu` for wallet operations, `intake`/`file_storage` for verified publication and cleanup for retention enforcement. The missing responsibility is durable purchase/entitlement coordination, shared by API and frontend, plus protected operator withdrawal. Do not prebuild accounts, quotas, multi-tenancy or a generalized payment-plugin system for the friends-only scenario.

## 10. Runtime evidence and limits

Observed against the current binary built with `cargo build --bin almond`, isolated temporary storage and fresh ephemeral Nostr keys:

- A 36-byte upload returned `201`; another key reuploaded identical bytes and received `200` with unchanged `uploaded` and `expiration`. Download returned the original bytes.
- `--upload-max-age 1h` capped a roughly two-hour client expiry to one hour; reupload with a later target did not extend it.
- Paid `HEAD /upload` returned `402` and a `cashuA` JSON challenge, not the required payment-request encoding.
- Paid startup created SQLite tables including `proof`, `keyset_counter`, `transactions`, `wallet_sagas`, plus a seed. A non-resolving mint sufficed for initialization/preflight; this proves no live mint compatibility.
- Restart with a one-file limit deleted the older upload before its announced expiration. The surviving blob's HEAD retained its original `Sunset`; duplicate reupload at capacity returned `507` before deduplication.
- Unauthenticated `/list` returned `401`; it was not used to establish persisted metadata.

Research processes were stopped and temporary blobs/wallet/seed removed. No implementation or permanent tests changed. No real mint settlement, external forwarding, withdrawal/refund, crash-in-flight recovery or live P2PK payment was exercised; those findings are source analysis and recommendations, not runtime proof.
