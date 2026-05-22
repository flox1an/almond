# BUD-11

## Privacy-Preserving Blob Discovery

`draft` `optional`

Defines the `/filter` endpoint for privacy-preserving blob existence queries using Binary Fuse filters.

## Motivation

The standard `HEAD /<sha256>` endpoint reveals to the server exactly which blobs a client is interested in. For users synchronizing large collections of files, this allows servers to build detailed profiles of client interests. This BUD specifies a privacy-preserving alternative where clients download a compact filter and perform all existence queries locally.

## Scope

The filter returned by this endpoint contains ALL blobs stored on the server, regardless of which user uploaded them. This is a global server-level filter, not a per-user ownership filter. The filter answers "does this blob exist on this server?" not "does this user own this blob?"

## Binary Fuse Filters

Binary Fuse filters are probabilistic data structures that answer set membership queries with the following properties:

- **No false negatives**: If the filter says a blob is "not present", it is definitely not present
- **Rare false positives**: The filter may occasionally report a blob as "maybe present" when it is not
- **Deterministic**: The same query against the same filter always returns the same result

This asymmetry is acceptable for blob deduplication: when the filter indicates "not present", the client uploads; when it indicates "maybe present", the client can skip the upload, knowing that a false positive merely delays the upload until the next filter rebuild.

### Filter Variants

Servers MUST support at least one of these variants:

| Variant | False Positive Rate | Size per element | Filter Type Header |
|---------|---------------------|------------------|-------------------|
| Binary Fuse8 | ~0.4% (1/256) | ~9 bits | `binary-fuse-8` |
| Binary Fuse16 | ~0.0015% (1/65536) | ~18 bits | `binary-fuse-16` |
| Binary Fuse32 | ~0.00000002% (1/2^32) | ~36 bits | `binary-fuse-32` |

Servers SHOULD prefer Binary Fuse16 as it offers a good balance between size and accuracy.

## GET /filter - Retrieve Filter

The `GET /filter` endpoint MUST return a JSON object containing the base filter and optional delta filters.

### Response Headers

Servers MUST include:

- `ETag`: A unique identifier for this filter version for caching purposes

Servers SHOULD include:

- `Cache-Control`: Caching directives (e.g., `max-age=3600`)

### Response Body

The response body MUST be a JSON object with the following structure:

```json
{
  "type": "binary-fuse-16",
  "timestamp": 1705320000,
  "count": 10485760,
  "filter": "<base64-encoded Binary Fuse filter>",
  "added": "<base64-encoded Binary Fuse filter>",
  "removed": "<base64-encoded Binary Fuse filter>"
}
```

- `type` (required): The filter variant used (`binary-fuse-8`, `binary-fuse-16`, or `binary-fuse-32`)
- `timestamp` (required): Unix timestamp (seconds since epoch) of when the base filter was generated
- `count` (required): Number of blob hashes encoded in the base filter
- `filter` (required): Base64-encoded Binary Fuse filter containing blob hashes known at the time of filter generation
- `added` (optional): Base64-encoded Binary Fuse filter of hashes added to the server since the base filter was generated
- `removed` (optional): Base64-encoded Binary Fuse filter of hashes deleted from the server since the base filter was generated

If `added` or `removed` are not present, clients MUST treat them as empty filters that return "not present" for all queries.

### Example Request

```http
GET /filter HTTP/1.1
Host: cdn.example.com
Accept: application/json
If-None-Match: "abc123"
```

### Example Response

```http
HTTP/1.1 200 OK
Content-Type: application/json
ETag: "abc123"
Cache-Control: max-age=3600

{
  "type": "binary-fuse-16",
  "timestamp": 1705320000,
  "count": 10485760,
  "filter": "SGVsbG8gV29ybGQh...",
  "added": "QW5vdGhlckZpbHRlcg==",
  "removed": "WWV0QW5vdGhlcg=="
}
```

### Conditional Requests

Servers SHOULD support conditional requests using `If-None-Match` headers. If the filter has not changed, servers MUST respond with `304 Not Modified`.

```http
HTTP/1.1 304 Not Modified
ETag: "abc123"
```

## Client Query Algorithm

Clients MUST check blob existence in the following order:

1. Query the `removed` filter - if it returns "maybe present", the blob does NOT exist on the server
2. Query the `added` filter - if it returns "maybe present", the blob DOES exist on the server
3. Query the base `filter` - if it returns "not present", the blob does NOT exist; if "maybe present", the blob probably exists

```
function blobExists(hash, filter, added, removed):
    if removed.contains(hash):
        return false
    if added.contains(hash):
        return true
    return filter.contains(hash)
```

Note: Since all three are probabilistic filters, there is a small chance of false positives. For the `removed` filter, a false positive means a client might skip uploading a blob that was deleted and re-needs uploading. For the `added` filter, a false positive means a client might skip uploading a blob that doesn't actually exist yet. Both cases resolve on the next filter rebuild cycle.

## Server Requirements

### Filter Generation

Servers MUST rebuild the filter periodically. The rebuild interval is implementation-specific but SHOULD be at least once per day for active servers.

When rebuilding the filter:

1. Generate a new Binary Fuse filter from all blob hashes currently stored
2. Clear the `added` and `removed` delta filters
3. Update the `timestamp` field and `ETag` header

### Delta Filter Maintenance

Between base filter rebuilds, servers MUST maintain delta filters:

- When a blob is uploaded, add its hash to the `added` filter
- When a blob is deleted, add its hash to the `removed` filter

Since Binary Fuse filters are immutable, servers MUST rebuild the delta filters each time a blob is added or removed. Given that delta filters are typically small (containing only changes since the last base filter rebuild), this rebuild cost is negligible.

Servers MAY trigger an early base filter rebuild if delta filters grow too large.

### Filter Authorization (Optional)

Servers MAY require authorization to access the filter endpoint.

If authorization is required, the server MUST accept a kind `24242` authorization event with:

1. The `t` tag set to `filter`
2. A valid `expiration` tag

Example authorization event:

```json
{
  "id": "8ecbdcdd5329200105524a14287913881b39d1409d8b90ccdb4b43f8f0fc9d0c",
  "pubkey": "9f0cc17023b2cf509e0f1d305793d20e7c7227692fd9bf855368887ac570a280",
  "kind": 24242,
  "content": "Get Filter",
  "created_at": 1708771227,
  "tags": [
    ["t", "filter"],
    ["expiration", "1708857540"]
  ],
  "sig": "02f0d2ab23b0444628..."
}
```

## Error Responses

If the server does not support filters, it MUST respond with:

```http
HTTP/1.1 404 Not Found
X-Reason: Filter endpoint not supported
```

If authorization is required but not provided:

```http
HTTP/1.1 401 Unauthorized
X-Reason: Authorization required
```

## Implementation Notes

### Libraries

Binary Fuse filter libraries are available for most languages:

- **C**: github.com/FastFilter/xor_singleheader
- **Go**: github.com/FastFilter/xorfilter
- **Rust**: docs.rs/xorf
- **JavaScript**: npm packages available

### Performance Characteristics

For reference, typical performance characteristics:

- Filter construction: ~35ms per million hashes
- Query time: ~25ns per lookup
- Space: ~18 bits per element for Binary Fuse16

### Backward Compatibility

Clients MAY continue using `HEAD /<sha256>` for individual blob existence checks. The filter endpoint is an optional optimization for bulk queries with privacy benefits.
