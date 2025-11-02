use std::time::Duration;

// File processing constants
pub const CHUNK_SIZE: usize = 1024 * 1024; // 1MB chunks
pub const LOG_INTERVAL: Duration = Duration::from_secs(5);
pub const MAX_THROUGHPUT_ENTRIES: usize = 1000;
pub const THROUGHPUT_CLEANUP_THRESHOLD: usize = 100;

// HTTP header names
pub const X_SHA_256_HEADER: &str = "X-SHA-256";
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
