use std::time::Duration;

// File processing constants
pub const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_THROUGHPUT_ENTRIES: usize = 1000;
pub const THROUGHPUT_CLEANUP_THRESHOLD: usize = 100;

/// Read-buffer size for streaming blobs off disk.
///
/// `ReaderStream`'s default is 4 KiB, and every read on a `tokio::fs::File` is
/// a dispatch to the blocking pool, so a 100 MB blob cost ~25.6k round trips
/// and as many `BytesMut` allocations. 128 KiB cuts that by 32x while keeping
/// per-connection buffer memory bounded.
pub const FILE_STREAM_BUFFER_SIZE: usize = 128 * 1024;

/// Floor for the adaptive buffer so small ranges do not over-allocate.
pub const FILE_STREAM_MIN_BUFFER_SIZE: usize = 8 * 1024;

// HTTP header names
pub const X_SHA_256_HEADER: &str = "X-SHA-256";
pub const X_EXPIRATION_HEADER: &str = "X-Expiration";
pub const UPLOAD_TYPE_HEADER: &str = "Upload-Type";
pub const UPLOAD_LENGTH_HEADER: &str = "Upload-Length";
pub const UPLOAD_OFFSET_HEADER: &str = "Upload-Offset";

// Default values
pub const DEFAULT_CONTENT_TYPE: &str = "application/octet-stream";
pub const DEFAULT_MIME_TYPE: &str = "application/octet-stream";

// Cache control
pub const CACHE_CONTROL_IMMUTABLE: &str = "public, max-age=31536000, immutable";

// HTTP client timeout constants
pub const HTTP_REQUEST_TIMEOUT_SECS: u64 = 30;
pub const HTTP_CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DNS_LOOKUP_TIMEOUT_SECS: u64 = 5;
pub const HTTP_REQUEST_MAX_REDIRECTS: u8 = 5;

// Shared upstream client tuning.
//
// An edge node talks to a small, fixed set of origins, so keeping connections
// warm removes a TCP + TLS handshake from every cache miss.
pub const UPSTREAM_POOL_MAX_IDLE_PER_HOST: usize = 32;
pub const UPSTREAM_POOL_IDLE_TIMEOUT_SECS: u64 = 90;
pub const UPSTREAM_TCP_KEEPALIVE_SECS: u64 = 60;
/// Inactivity timeout on the upstream body stream. Deliberately *not* a total
/// request timeout: multi-hundred-megabyte blobs legitimately take minutes.
pub const UPSTREAM_READ_TIMEOUT_SECS: u64 = 30;
