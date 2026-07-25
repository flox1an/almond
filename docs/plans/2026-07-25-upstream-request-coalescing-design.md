# Upstream Request Coalescing Design

## Problem

`try_upstream_servers` (`src/handlers/upstream.rs:66-89`) checks whether the hash is
already in `ongoing_downloads`, and if it is, calls `proxy_request_to_upstream` — a
*fresh, independent* origin GET. The in-flight download is ignored entirely.

Cost on a cold object with `N` concurrent clients, `R` of them ranged:

| Path | Origin GETs today | Target |
|---|---|---|
| `N` non-range clients | `N` | 1 |
| 1 cold range client | 2 (ranged proxy + full background fetch) | 1 |
| `N` clients, `R` ranged | `N + R` | 1 |

The machinery to fix this already exists — `create_tailing_stream` +
`written_len`/`Notify` (`upstream.rs:1709-1752`) — but it is constructed inline in
`stream_and_save_from_upstream` and handed only to the initiating request. Nothing
else can attach.

## Blockers in the current machinery

Attaching followers is *not* pure plumbing. Four latent defects are masked today
because exactly one reader exists and it always knows `Content-Length`.

### 1. The tailing stream has no terminal state

`create_tailing_stream` loops forever on `notify.notified()`. It never ends. It
only appears to work because the initiator's response carries upstream's
`Content-Length` (`upstream.rs:1414-1421`), so hyper stops reading at `N` bytes.

Consequences that become the norm once followers exist:
- upstream without `Content-Length` → response hangs until client timeout;
- download failure mid-body → client gets a silently **truncated `200`**, no error;
- panic in the finalize task → every reader hangs forever.

### 2. Lost wakeup in `Notify`

`notify_waiters()` wakes only tasks *already registered*; it leaves no permit.
The reader does:

```rust
let available = written_len.load(Acquire);   // sees N
if pos < available { ... } else { notify.notified().await }   // downloader fired here
```

A `notify_waiters()` landing between the load and the `await` is lost. If that was
the **last** chunk, the reader sleeps forever. Masked today by `Content-Length`
termination.

### 3. `written_len` does not imply "readable"

The downloader publishes progress right after `write_all`
(`upstream.rs:1371-1376`). `tokio::fs::File` buffers internally: `write_all`
returns once the write is *submitted* to the blocking pool, not once it lands.
So a reader can be told `written = N` and read back fewer than `N` bytes. Today
the reader treats a short read as `n == 0` → wait → retry, which self-heals. Any
follower logic that trusts `written` as a read barrier is racy.

### 4. temp → final rename races the map entry

Finalize renames temp → final (`upstream.rs:1453`) and only *then* removes the
`ongoing_downloads` entry (`upstream.rs:1503`). A follower that reads the map
between those two points opens a path that no longer exists.

Plus: `download_file_from_upstream_background` (`upstream.rs:1539-1706`) never
touches `written_len`/`notify` at all, so redirect-mode and range-triggered
background downloads publish no progress and cannot be followed. And every one of
its ~12 error paths hand-rolls `ongoing_downloads.remove()`; a panic on any of
them leaks the entry permanently.

## Design

### Slot type

Replace the 5-tuple in `models.rs:176-177`:

```rust
type OngoingDownloadsMap =
    Arc<RwLock<HashMap<String, (Instant, Arc<AtomicU64>, Arc<Notify>, PathBuf, String)>>>;
```

with a named handle carrying a terminal state:

```rust
#[derive(Clone, Copy, PartialEq)]
pub enum Phase { Running, Done, Failed }

#[derive(Clone, Copy)]
pub struct Progress { pub written: u64, pub phase: Phase }

pub struct DownloadHandle {
    pub started: Instant,
    pub temp_path: PathBuf,
    pub content_type: String,
    /// Upstream `Content-Length`. `None` ⇒ followers cannot serve ranges.
    pub total_len: Option<u64>,
    pub progress: watch::Sender<Progress>,
}

pub type OngoingDownloadsMap = Arc<RwLock<HashMap<String, Arc<DownloadHandle>>>>;
```

`watch` instead of `AtomicU64 + Notify` fixes blocker 2 by construction: the
receiver holds a version, so `changed()` cannot miss an update that happened
before the await. It also carries `Phase`, fixing blocker 1 — a follower observes
`Done` and ends its stream, or `Failed` and errors it.

`total_len` is immutable: the slot is only created once an upstream `2xx` is in
hand, so it needs no lock.

### Write barrier

Downloader becomes:

```rust
writer.write_all(&chunk).await?;
writer.flush().await?;                 // sync with the blocking pool, not fsync
progress.send_modify(|p| p.written += chunk.len() as u64);
```

`flush()` on `tokio::fs::File` waits for the in-flight write op — it is not an
`fsync`, so no disk barrier. This makes "`written` bytes are readable" a real
invariant (blocker 3) and lets the tail stream treat a short read as an error
instead of a retry.

### Cleanup guard

One RAII guard replaces the ~12 hand-rolled `remove()` sites:

```rust
struct DownloadGuard { state: AppState, key: String, handle: Arc<DownloadHandle> }

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        // Unblock followers before the entry disappears.
        self.handle.progress.send_if_modified(|p| {
            if p.phase == Phase::Running { p.phase = Phase::Failed; true } else { false }
        });
        let (state, key) = (self.state.clone(), std::mem::take(&mut self.key));
        tokio::spawn(async move { state.ongoing_downloads.write().await.remove(&key); });
    }
}
```

Drop runs on panic, so a leaked slot can no longer hang every future request for
that hash. The success path sets `Phase::Done` before the guard drops.

### Tail stream

```rust
fn tail_stream(
    reader: File,
    rx: watch::Receiver<Progress>,
    start: u64,
    end: Option<u64>,   // exclusive
) -> impl Stream<Item = io::Result<Bytes>>
```

Loop:
1. `let p = *rx.borrow_and_update();`
2. `limit = end.unwrap_or(u64::MAX).min(p.written)`
3. `pos >= limit` → match `p.phase`:
   - `Running` → `rx.changed().await`, continue;
   - `Done` → `None` if `end` is satisfied, else `Some(Err(...))` (upstream
     delivered fewer bytes than promised — reset the stream rather than ship a
     truncated `200`);
   - `Failed` → `Some(Err(...))`.
4. else read `min(64 KiB, limit - pos)` and yield.

Seek once at construction, not per chunk — the fd position only moves by our own
reads, so the per-chunk `seek` at `upstream.rs:1725` is a wasted syscall per 64 KiB.

### Follower entry point

```rust
/// Serve `file_hash` from an in-flight download. `None` ⇒ caller falls back to
/// its own origin fetch.
async fn try_serve_from_ongoing(
    state: &AppState,
    file_hash: &str,
    filename: &str,
    headers: &HeaderMap,
) -> Option<Response>
```

1. Clone the `Arc<DownloadHandle>` out of the map, drop the read guard immediately.
2. `File::open(&handle.temp_path).await.ok()?` — **before** any await on progress.
   `ENOENT` ⇒ already renamed or dead ⇒ `None`, and the caller re-checks
   `file_index` first. On Unix the rename cannot invalidate an fd we already hold,
   so this closes blocker 4.
3. Dispatch on the range header, reusing `parse_range_header` against
   `handle.total_len`:
   - no range → `200`, `Content-Length: total_len`, `tail_stream(0, None)`;
   - `Satisfiable { start, end }` → `206`, `Content-Range`/`Content-Length`,
     `tail_stream(start, Some(end + 1))`;
   - `total_len == None` **and** a range is present → `None`. `Content-Range`
     requires the total; proxy that one request.
   - already fully written (`start >= written`… no: `end < written`) → skip the
     tail entirely, plain seek + `take`, identical to `serve_file_with_range`.

### Far-seek policy

Waiting is not free. A player seeking to 00:45:00 of a file we have streamed 3 MB
of would block until the sequential download reaches that offset. That is worse
for the user than one extra origin request.

Rule: if `start > written + SEEK_AHEAD_LIMIT` (default 8 MiB) and phase is
`Running`, return `None` → the request is proxied to origin as it is today. Every
other case attaches. Sequential playback (`bytes=0-`, then contiguous seeks) is
the common case and coalesces fully; pathological far seeks keep today's latency.

### Backpressure

Followers read from the file, never from a shared buffer, so a slow client cannot
throttle the origin fetch and cannot grow memory. Keep this property — it is why
file-tailing beats a `broadcast::Sender<Bytes>` here, which would also need
backfill logic for followers that join mid-stream.

An initiator disconnecting does not abort the download: `download_task` is a
detached spawn (`upstream.rs:1349`) writing to the file, independent of the
response stream. Followers are unaffected. This already holds today.

## Phases

| Phase | Change | Origin GETs after |
|---|---|---|
| 0 | `DownloadHandle` + `watch` + `DownloadGuard`; fold temp-file creation into `prepare_download_state`; unify `stream_and_save_from_upstream` / `download_file_from_upstream_background` onto one `run_download` core; flush-before-publish | unchanged |
| 1 | `try_serve_from_ongoing`, wired at `upstream.rs:66-89` in place of the proxy call | `N` non-range clients → 1 |
| 2 | Cold range request issues one full GET and serves the client's range as a follower of its own download | `N + R` → 1 |
| 3 | Single-flight across the negotiation window: insert the slot with `Phase::Negotiating` *before* the origin GET, so requests arriving during server negotiation wait instead of racing | closes the last duplicate-fetch window |
| 4 | Answer `HEAD` from the slot when `total_len` is known; add an `almond_upstream_requests_coalesced_total` counter | `HEAD` → 0 |

Phase 0 is a prerequisite for everything and has no independent slices — it runs
first, alone. Phases 1 and 2 are the value.

Phase 3 needs a bounded wait: a follower parked on a `Negotiating` slot must time
out and run its own lookup if negotiation stalls across several dead servers.

## Metrics

- Followers must call `metrics.track_download(bytes_served)`; only the downloader
  calls `track_upstream_download`. Today `download_file_from_upstream_background`
  bumps `files_downloaded` directly (`upstream.rs:1691`) — route it through the
  same place.
- `almond_upstream_requests_coalesced_total` is the proof the change works.

## Verification

No integration harness exists (unit tests are inline `#[cfg(test)]` modules only).

- Unit: `tail_stream` against a hand-driven `watch::Sender` — mid-stream join,
  join after `Done`, `Failed` mid-body yields `Err`, `end` bound respected,
  short-read-at-`Done` yields `Err` not a truncated success.
- Smoke: run almond against a local origin serving a large blob with an
  artificial rate limit; fire `N` concurrent `curl`s (mixed range and non-range)
  at a cold hash; assert origin access-log line count is 1 and every client's
  output hashes to the expected SHA-256.
