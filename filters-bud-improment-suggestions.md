 Clarity & Completeness

  1. Specify hash format - The spec doesn't define what format hashes should be in when added to filters. Should it be raw 32 bytes?
   Hex-encoded? This is critical for interoperability.
  2. Add a version field - Consider adding "version": 1 to the response for future evolution of the spec.
  3. Define "too large" for delta filters - The spec says servers "MAY trigger an early base filter rebuild if delta filters grow
  too large" but doesn't provide guidance. A threshold like "when delta filter size exceeds 10% of base filter" would help
  implementers.

  Performance

  4. Support compression - Base64 adds ~33% overhead. Consider recommending Accept-Encoding: gzip support, or offering a binary
  endpoint variant (/filter?format=binary).
  5. Delta filter maintenance cost - Rebuilding Binary Fuse filters on every upload/delete is expensive. Consider allowing servers
  to use simpler data structures (bloom filters, hash sets) for deltas, or batch delta updates.

  Edge Cases

  6. False positive in removed filter is problematic - If removed.contains(hash) gives a false positive, the client thinks the blob
  doesn't exist when it does. This causes unnecessary uploads. Consider noting this asymmetry or recommending fuse32 for the removed
   filter specifically.
  7. Race condition handling - What should clients do if they skip an upload due to filter saying "exists", but the blob gets
  deleted before they reference it? A note on this edge case would help.

  Security

  8. Rate limiting guidance - For a privacy-focused endpoint, rate limiting considerations matter. Recommend servers implement
  reasonable limits.
  9. Authorization event requirements - Clarify if the content field in the auth event has any required value or is freeform.

  Would you like me to draft updated text for any of these?

Questions:

- public or do we need auth?
- Should we store the data as binary in link to it? blob with expiration?

