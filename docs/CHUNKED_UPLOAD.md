# Chunked Upload Implementation (BUD-10)

This implementation adds support for multi-part uploads according to the BUD-10 specification.

## Features

- **OPTIONS /upload**: Signals support for multi-part uploads with `Allow: PUT, HEAD, OPTIONS, PATCH` header
- **PATCH /upload**: Handles chunked uploads with proper validation and reconstruction
- **Authorization**: Full Nostr authorization support for chunked uploads
- **Chunk validation**: Ensures chunks are properly ordered and cover the full upload length
- **SHA256 verification**: Validates the final reconstructed blob matches the expected hash

## API Endpoints

### OPTIONS /upload

Returns `204 No Content` with `Allow: PUT, HEAD, OPTIONS, PATCH` header to signal multi-part upload support.

### PATCH /upload

Uploads a chunk of a multi-part upload. Requires the following headers:

- `X-SHA-256`: SHA256 hash of the final blob
- `Upload-Type`: MIME type of the final blob
- `Upload-Length`: Total length of the blob
- `Content-Length`: Length of the current chunk
- `Upload-Offset`: Offset of the chunk in the blob
- `Content-Type`: Must be `application/octet-stream`
- `Authorization`: Nostr authorization event

#### Response Codes

- `204 No Content`: Chunk accepted, upload not complete
- `200 OK`: Upload complete, returns blob descriptor
- `400 Bad Request`: Invalid headers or chunk data
- `401 Unauthorized`: Invalid or missing authorization
- `500 Internal Server Error`: Server error during processing

## Authorization

The Nostr authorization event must include:

- `t` tag set to `upload`
- `x` tags for each chunk with the SHA256 hash of the chunk
- `x` tag with the SHA256 hash of the final blob

## Example Usage

```bash
# Check if chunked uploads are supported
curl -X OPTIONS http://localhost:3000/upload

# Upload first chunk
curl -X PATCH http://localhost:3000/upload \
  -H "X-SHA-256: b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553" \
  -H "Upload-Type: application/pdf" \
  -H "Upload-Length: 184292" \
  -H "Upload-Offset: 0" \
  -H "Content-Length: 46073" \
  -H "Content-Type: application/octet-stream" \
  -H "Authorization: Nostr <base64-encoded-event>" \
  --data-binary "@chunk_aa"

# Upload remaining chunks...
curl -X PATCH http://localhost:3000/upload \
  -H "X-SHA-256: b1674191a88ec5cdd733e4240a81803105dc412d6c6708d53ab94fc248f4f553" \
  -H "Upload-Type: application/pdf" \
  -H "Upload-Length: 184292" \
  -H "Upload-Offset: 46073" \
  -H "Content-Length: 46073" \
  -H "Content-Type: application/octet-stream" \
  -H "Authorization: Nostr <base64-encoded-event>" \
  --data-binary "@chunk_ab"
```

## Implementation Details

### Data Structures

- `ChunkUpload`: Tracks ongoing chunked uploads
- `ChunkInfo`: Stores individual chunk data and metadata
- In-memory storage of chunk data during upload process

### Chunk Processing

1. **Validation**: Headers and chunk data are validated
2. **Storage**: Chunks are stored in memory with their offset and data
3. **Reconstruction**: When upload is complete, chunks are sorted by offset and reconstructed
4. **Verification**: Final blob SHA256 is verified against expected hash
5. **Persistence**: Reconstructed blob is saved to final location and indexed

### Error Handling

- Invalid headers return `400 Bad Request`
- Missing or invalid authorization returns `401 Unauthorized`
- Chunk size mismatches return `400 Bad Request`
- SHA256 verification failures return `500 Internal Server Error`
- Gaps in chunk coverage return `500 Internal Server Error`

## Testing

Use the provided `test_chunked_upload.py` script to test the implementation:

```bash
python3 test_chunked_upload.py
```

The test script:
1. Creates a test file
2. Splits it into chunks
3. Uploads chunks via PATCH requests
4. Verifies the final result

## Configuration

No additional configuration is required. The chunked upload functionality uses the same storage and authorization settings as regular uploads.
