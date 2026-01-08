# Almond - Context Documentation

This document provides a comprehensive overview of the Almond project, compiled from all documentation files.

## Project Overview

**Almond** (Any Large Media ON Demand) is a temporary BLOSSOM file storage service with Nostr-based authorization and web of trust support.

### Key Characteristics
- 🌸 Implements Blossom API (BUD-1, BUD-2, BUD-4, BUD-7, BUD-10)
- 🌸 Temporary file storage with automatic cleanup (FIFO)
- 🌸 No ownership tracking, no manual delete (except via DELETE endpoint)
- 🌸 Filesystem-only storage, no database
- 🌸 Web of Trust authorization support
- 🌸 Upstream server fallback for caching edge scenarios

### Use Cases
1. **Personal server** locked to one or a few users (`ALLOWED_NPUBS`)
2. **Public upload server** with limited TTL (`MAX_FILE_AGE_DAYS`) or size (`MAX_TOTAL_SIZE`)
3. **Caching edge server** that serves content from upstream blossom servers (`UPSTREAM_SERVERS`)

## API Endpoints

### File Operations (Require Authentication)

#### `PUT /upload`
- **Purpose**: Upload a file (BUD-1)
- **Auth**: ✅ Required (Nostr Kind 24242)
- **Feature Flag**: `FEATURE_UPLOAD_ENABLED` (default: `true`)
- **Whitelist**: Optional (if `ALLOWED_NPUBS` is set)

#### `PATCH /upload`
- **Purpose**: Chunked upload (BUD-2, BUD-10)
- **Auth**: ✅ Required (Nostr Kind 24242)
- **Feature Flag**: `FEATURE_UPLOAD_ENABLED` (default: `true`)
- **Headers Required**:
  - `X-SHA-256`: SHA256 hash of final blob
  - `Upload-Type`: MIME type of final blob
  - `Upload-Length`: Total length of blob
  - `Upload-Offset`: Offset of chunk in blob
  - `Content-Type`: Must be `application/octet-stream`
- **Response**: `204 No Content` (chunk accepted) or `200 OK` (upload complete)

#### `PUT /mirror`
- **Purpose**: Mirror a file from another server (BUD-4)
- **Auth**: ✅ Required (Nostr Kind 24242)
- **Feature Flag**: `FEATURE_MIRROR_ENABLED` (default: `true`)
- **Body**: JSON with `{"url": "https://..."}`
- **Security**: SSRF protection (HTTPS only, no private IPs)

#### `DELETE /:filename`
- **Purpose**: Delete a blob by SHA-256 hash
- **Auth**: ✅ Required (STRICT MODE - no WOT)
- **Whitelist**: ✅ **REQUIRED** (`ALLOWED_NPUBS` must be set)
- **Auth Event Requirements**:
  - `t` tag with value `"delete"`
  - `x` tag matching SHA-256 hash of blob
  - Pubkey must be in `ALLOWED_NPUBS` (WOT not allowed)
- **Response**: `204 No Content` on success

### File Operations (Public)

#### `GET /:filename` and `HEAD /:filename`
- **Purpose**: Download file by SHA256 hash
- **Auth**: ❌ Not required
- **Query Parameters**:
  - `?origin=<server>`: Custom upstream origin (requires `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED=true`)
  - `?xs=<server1>&xs=<server2>`: Multiple upstream servers (BUD-01)
  - `?as=<pubkey>`: Author pubkey for logging
- **Features**: Range request support, upstream fallback

#### `GET /list` and `GET /list/:id`
- **Purpose**: List all stored files
- **Auth**: ❌ Not required (publicly accessible)
- **Feature Flag**: `FEATURE_LIST_ENABLED` (default: `true`)
- **Query Parameters**: `?since=` and `?until=` (unix timestamps for filtering)

### System Information (Public)

#### `GET /_stats`
- **Purpose**: Server statistics and performance metrics
- **Auth**: ❌ Not required
- **Response**: JSON with upload/download counts, throughput, storage usage

#### `GET /_upstream`
- **Purpose**: Get configured upstream servers information
- **Auth**: ❌ Not required
- **Response**: JSON with upstream server list and max download size

#### `GET /_bloom`
- **Purpose**: Get Bloom filter over all stored SHA-256 hashes
- **Auth**: ❌ Not required
- **Response**: Bloom filter data

#### `GET /` and `GET /index.html`
- **Purpose**: Homepage/landing page
- **Auth**: ❌ Not required
- **Feature Flag**: `FEATURE_HOMEPAGE_ENABLED` (default: `true`)

#### `OPTIONS /upload`
- **Purpose**: CORS preflight request
- **Auth**: ❌ Not required
- **Response**: `Allow: PUT, HEAD, OPTIONS, PATCH` header

## Authentication & Authorization

### Nostr Authentication

All write operations require a Nostr authorization event:

```
Authorization: Nostr <base64-encoded-event>
```

### Event Requirements

1. **Kind**: Must be `24242`
2. **Signature**: Must be valid (`event.verify()`)
3. **Expiration**: Must not be expired (check `expiration` tag)
4. **Pubkey Authorization**: 
   - If `ALLOWED_NPUBS` is set: Pubkey must be in whitelist OR in `trusted_pubkeys` (WOT)
   - If `ALLOWED_NPUBS` is empty: Any authenticated pubkey is accepted

### Auth Modes

#### Standard Mode (Uploads, Mirror)
- ✅ Allows Web of Trust (`trusted_pubkeys`)
- ✅ Falls back to WOT if pubkey not in whitelist
- ⚠️ Requires `ALLOWED_NPUBS` only if whitelist is desired

#### Strict Mode (Delete)
- ❌ **NO Web of Trust** - WOT is disabled
- ✅ **REQUIRES** `ALLOWED_NPUBS` to be set
- ✅ Only explicitly whitelisted pubkeys can delete
- Returns `403 Forbidden` if `ALLOWED_NPUBS` is not configured

### Web of Trust (WOT)

- Automatically enabled when any feature is set to `wot` mode
- Uses Nostr follower graphs to determine trusted pubkeys
- Built from `ALLOWED_NPUBS` using a 2-hop graph from Nostr relays
- Only applies to upload/mirror operations (not delete)
- `trusted_pubkeys` are refreshed every 4 hours via `refresh_trust_network()`

## Cashu Payments (BUD-07)

When paid features are enabled, the server implements the BUD-07/NUT-24 payment flow.

### Two Payment Flows Supported

**1. Reactive Flow (402 round-trip):**
1. Client makes request without payment
2. Server returns `HTTP 402 Payment Required` with `X-Cashu` header
3. Client obtains Cashu token from accepted mint
4. Client retries request with `X-Cashu: cashuB...` header
5. Server verifies token, receives it into wallet, proceeds

**2. Preemptive Flow (single request):**
1. Client calculates expected price (size_mb * CASHU_PRICE_PER_MB)
2. Client includes `X-Cashu: cashuB...` header with first request
3. Server validates amount is sufficient for actual size
4. If sufficient: receives token, proceeds with operation
5. If insufficient: returns 402 with remaining amount needed

### Price Discovery (HEAD /upload)

Clients can query pricing before uploading:
```
HEAD /upload
```
Response headers (when paid uploads enabled):
- `X-Price-Per-MB`: Price in sats per megabyte
- `X-Price-Unit`: Currency unit (always "sat")
- `X-Accepted-Mints`: Comma-separated list of accepted mint URLs

### Payment Request Format (X-Cashu header on 402 response)
```json
{
  "a": 10,           // Amount in sats
  "u": "sat",        // Unit
  "m": ["https://mint.example.com"]  // Accepted mints
}
```

### Payment Proof Format (X-Cashu header on request)
Standard Cashu token string starting with `cashuA` or `cashuB`.

## Configuration

### Environment Variables

#### Server Configuration
- `BIND_ADDR`: Address to bind server (default: `"127.0.0.1:3000"`)
- `PUBLIC_URL`: Public URL for service (default: `"http://127.0.0.1:3000"`)

#### Storage Configuration
- `STORAGE_PATH`: Path where files are stored (default: `"./files"`)
- `MAX_TOTAL_SIZE`: Maximum total storage size in MB (default: `99999`)
- `MAX_TOTAL_FILES`: Maximum number of files (default: `99999999`)
- `CLEANUP_INTERVAL_SECS`: Interval for cleanup checks in seconds (default: `30`)
- `MAX_FILE_AGE_DAYS`: Maximum age of files in days, 0 for no limit (default: `0`)

#### Upstream Configuration
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers for file fallback
- `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`: Maximum size for upstream downloads in MB (default: `100`)

#### Chunked Upload Configuration
- `MAX_CHUNK_SIZE_MB`: Maximum size for individual chunks in MB (default: `100`)
- `CHUNK_CLEANUP_TIMEOUT_MINUTES`: Timeout for cleaning up abandoned chunked uploads (default: `30`)

#### Authorization Configuration
- `ALLOWED_NPUBS`: Comma-separated list of allowed Nostr pubkeys
  - Used as whitelist with WOT as fallback for uploads/mirrors
  - **Required** for delete operations
  - Used as seed for WOT 2-hop graph when WOT mode is enabled

#### Cashu Payment Configuration (BUD-07)
- `FEATURE_PAID_UPLOAD`: Enable paid uploads - `off` or `on` (default: `off`)
- `FEATURE_PAID_MIRROR`: Enable paid mirrors - `off` or `on` (default: `off`)
- `FEATURE_PAID_DOWNLOAD`: Enable paid downloads - `off` or `on` (default: `off`)
- `CASHU_PRICE_PER_MB`: Price per megabyte in satoshis (default: `1`)
- `CASHU_ACCEPTED_MINTS`: Comma-separated list of accepted Cashu mint URLs (required if any paid feature enabled)
- `CASHU_WALLET_PATH`: Path to SQLite wallet database (default: `./cashu_wallet.db`)

#### Feature Flags
- `FEATURE_UPLOAD_ENABLED`: Upload endpoint mode - `off`, `wot`, or `public` (default: `public`)
- `FEATURE_MIRROR_ENABLED`: Mirror endpoint mode - `off`, `wot`, or `public` (default: `public`)
- `FEATURE_LIST_ENABLED`: Enable list endpoint (default: `true`)
- `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED`: Custom upstream origin mode - `off`, `wot`, or `public` (default: `off`)
  - Controls `?origin=`, `?xs=`, and `?as=` URL parameters
  - In `wot` mode, validates `?as=` author pubkey against Web of Trust
- `FEATURE_HOMEPAGE_ENABLED`: Enable homepage/landing page (default: `true`)

## Architecture & Code Structure

### Service Layer Architecture

The codebase has been refactored to use a service layer pattern:

#### `src/error.rs`
- Custom `AppError` enum with typed error variants
- Automatic conversion to HTTP `StatusCode`
- Implements `IntoResponse` for Axum compatibility

#### `src/services/auth.rs`
- Authentication and authorization logic
- Functions: `parse_auth_header`, `verify_event`, `check_pubkey_authorization`
- Two auth modes: `Standard` (with WoT) and `Strict` (whitelist only)
- Hash extraction from events

#### `src/services/file_storage.rs`
- File operations: `get_nested_path`, `move_file`, `delete_file`
- Index management: `add_to_index`, `remove_from_index`, `get_file_metadata`
- Helper functions: `ensure_temp_dir`, `create_temp_path`
- Validation: `validate_sha256_format`, `extract_sha256_from_filename`

#### `src/services/upload.rs`
- SSRF protection: `validate_url_for_ssrf`, `is_private_ip`
- HTTP client: `create_hardened_http_client`
- Streaming: `stream_to_temp_file`, `stream_response_to_temp_file`
- Upload finalization: `finalize_upload`
- Network operations: `fetch_from_url`, `check_size_limit`

#### `src/services/download.rs`
- Download state tracking: `prepare_download_state`
- State management: `remove_from_ongoing_downloads`, `is_download_in_progress`
- Failed lookup caching: `mark_failed_lookup`, `is_recently_failed`

### File Storage Structure

Files are stored in a two-level directory hierarchy based on SHA-256 hash:

```
./files/<first_char>/<second_char>/<hash>[.<ext>]
```

Example:
```
./files/5/3/53860ca3a463ad7170fe1f1e5b08bf4b66422c72b594a329e001a69e07f2e50e.mp4
```

### Code Quality Improvements

#### Refactoring Metrics
- `mirror_blob`: 234 lines → 110 lines (-53%)
- `upload_file`: 139 lines → ~60 lines (-57%)
- Average function size: 50 lines → 20 lines (-60%)
- Code duplication: ~200 lines → 0 lines (-100%)

#### Benefits
- ✅ Separation of concerns (handlers focus on HTTP, services on business logic)
- ✅ Code reusability across handlers
- ✅ Better testability (each layer can be tested independently)
- ✅ Type-safe error handling with `AppError`
- ✅ Consistent patterns throughout codebase

## Security Considerations

### Implemented Security Features

1. **SSRF Protection** (for `/mirror` endpoint)
   - HTTPS only (no HTTP)
   - Private IP blocking (10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16)
   - DNS resolution and IP validation
   - Hardened HTTP client with timeouts and redirect limits

2. **Nostr Authentication**
   - Signature verification
   - Event expiration checking
   - Pubkey whitelist support
   - Web of Trust support (for uploads)

3. **Upload Size Limits**
   - Global body limit via `DefaultBodyLimit`
   - Per-upload size limits
   - Streaming to disk (prevents memory exhaustion)

4. **Chunked Upload Security**
   - Chunk validation and ordering
   - SHA256 verification of final blob
   - Temporary file cleanup for abandoned uploads

### Security TODOs (High Priority)

1. **Harden `mirror_blob`**
   - ✅ Already implemented: HTTPS only, private IP blocking, streaming
   - ⚠️ TODO: Optional domain allowlist via `MIRROR_ALLOWLIST` ENV

2. **Restrict CORS**
   - ⚠️ TODO: Implement `ALLOWED_ORIGINS` ENV variable
   - ⚠️ TODO: Return origin-specific `Access-Control-Allow-Origin`
   - ⚠️ TODO: Add `Vary: Origin` header

3. **Upload Stream Limits**
   - ⚠️ TODO: Add `MAX_SINGLE_UPLOAD_MB` ENV variable
   - ⚠️ TODO: Enforce during streaming (abort on limit)

4. **Protect `/list` Endpoint**
   - ⚠️ TODO: Add `PUBLIC_LIST=false` flag
   - ⚠️ TODO: Optional Nostr auth requirement
   - ⚠️ TODO: Rate limiting

5. **Security Headers**
   - ⚠️ TODO: Add `X-Content-Type-Options: nosniff`
   - ⚠️ TODO: Add `Referrer-Policy: no-referrer`
   - ⚠️ TODO: Add `Permissions-Policy` header
   - ⚠️ TODO: Strict CSP for index page

6. **Upstream Proxying**
   - ✅ Already robust with size limits
   - ⚠️ TODO: Filter hop-by-hop headers (Connection, Transfer-Encoding, etc.)

7. **Rate Limiting**
   - ⚠️ TODO: Per-IP rate limiting
   - ⚠️ TODO: Per-pubkey rate limiting

8. **Disk Space Checks**
   - ⚠️ TODO: Preflight free-space checks before accepting uploads

## Development

### Prerequisites
- Rust 1.76 or later
- OpenSSL development libraries

### Building
```bash
cargo build --release
```

### Running
```bash
cargo run --release
```

### Docker

#### Building
```bash
docker build -t almond .
```

#### Running
```bash
docker run -p 3000:3000 \
  -v /path/to/files:/app/files \
  -e STORAGE_PATH=/app/files \
  -e PUBLIC_URL=https://your-domain.com \
  -e FEATURE_UPLOAD_ENABLED=wot \
  -e FEATURE_MIRROR_ENABLED=wot \
  -e ALLOWED_NPUBS=npub1... \
  -e MAX_TOTAL_SIZE=1000 \
  -e MAX_FILE_AGE_DAYS=7 \
  -e UPSTREAM_SERVERS=https://backup1.com,https://backup2.com \
  ghcr.io/flox1an/almond
```

### Docker Build Optimization

The Dockerfile uses BuildKit cache mounts for faster builds:
- Cargo registry cache (`/usr/local/cargo/registry`)
- Target directory cache (`/usr/src/app/target`)
- Dependency layer caching (rebuilds only when `Cargo.toml`/`Cargo.lock` changes)

Expected build times:
- First build: ~5 minutes
- Source changes only: ~1-2 minutes
- No changes: ~30 seconds

## Chunked Upload (BUD-10)

### Implementation Details

1. **Chunk Storage**: Chunks are streamed directly to temporary files (prevents memory exhaustion)
2. **Chunk Validation**: Headers and chunk data are validated
3. **Reconstruction**: When upload is complete, chunks are sorted by offset and read from files
4. **Verification**: Final blob SHA256 is verified against expected hash
5. **Persistence**: Reconstructed blob is saved to final location and indexed
6. **Cleanup**: Temporary chunk files are automatically cleaned up

### Data Structures
- `ChunkUpload`: Tracks ongoing chunked uploads
- `ChunkInfo`: Stores individual chunk metadata and file paths

### Error Handling
- Invalid headers: `400 Bad Request`
- Missing/invalid authorization: `401 Unauthorized`
- Chunk size mismatches: `400 Bad Request`
- SHA256 verification failures: `500 Internal Server Error`
- Gaps in chunk coverage: `500 Internal Server Error`

## Upstream Server Fallback

### Features
- Automatic fallback to upstream servers when file not found locally
- Failed lookup caching (1 hour TTL) to prevent repeated upstream queries
- Support for custom origin via `?origin=` query parameter
- Support for multiple upstream servers via `?xs=` query parameters (BUD-01)
- Range request support for upstream proxying
- Background download while proxying range requests

### Configuration
- `UPSTREAM_SERVERS`: Comma-separated list of upstream servers
- `MAX_UPSTREAM_DOWNLOAD_SIZE_MB`: Maximum size for upstream downloads
- `FEATURE_CUSTOM_UPSTREAM_ORIGIN_ENABLED`: Enable custom origin parameter

## File Cleanup

### Automatic Cleanup
- **Expiration-based**: Files with `X-Expiration` header are deleted when expired
- **Age-based**: Files older than `MAX_FILE_AGE_DAYS` are deleted
- **Size-based**: Oldest files deleted when `MAX_TOTAL_SIZE` or `MAX_TOTAL_FILES` exceeded
- **Empty directories**: Automatically removed after file deletion

### Cleanup Process
1. Runs periodically based on `CLEANUP_INTERVAL_SECS`
2. Sorts files by creation date (oldest first)
3. Deletes expired/aged files first
4. Then enforces storage limits
5. Finally cleans up empty directories

## License

MIT





