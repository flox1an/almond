# Local Blossom Cache

Almond can run as a [Local Blossom Cache](https://github.com/hzrd149/blossom/blob/master/implementations/local-blossom-cache.md):
a local proxy on `127.0.0.1:24242` that caches blobs from remote Blossom servers.
Uploads and mirrors are off; the cache fills only through proxying.

```dotenv
ALMOND_BIND_ADDR=127.0.0.1:24242
ALMOND_PUBLIC_URL=http://127.0.0.1:24242
ALMOND_UPLOAD_ACCESS=off
ALMOND_MIRROR_ACCESS=off
ALMOND_LIST_ENABLED=true
ALMOND_HOMEPAGE_ENABLED=true
ALMOND_CUSTOM_ORIGIN_ACCESS=public
ALMOND_UPSTREAM_MODE=proxy
ALMOND_STORAGE_MAX_SIZE=5000MiB
ALMOND_UPSTREAM_CACHE_TTL=30d
```

With Docker:

```bash
docker run -p 24242:24242 \
  -v /path/to/cache:/app/files \
  -e ALMOND_BIND_ADDR=0.0.0.0:24242 \
  -e ALMOND_PUBLIC_URL=http://127.0.0.1:24242 \
  -e ALMOND_UPLOAD_ACCESS=off \
  -e ALMOND_MIRROR_ACCESS=off \
  -e ALMOND_CUSTOM_ORIGIN_ACCESS=public \
  -e ALMOND_UPSTREAM_MODE=proxy \
  -e ALMOND_STORAGE_MAX_SIZE=5000MiB \
  -e ALMOND_UPSTREAM_CACHE_TTL=30d \
  ghcr.io/flox1an/almond
```

A request flows like this:

1. Client requests `GET /abc123...def.jpg?xs=cdn.example.com&as=<pubkey>`.
2. A cached blob is returned immediately.
3. Otherwise Almond tries the `xs` hints, then the `as` author's BUD-03 server list
   (kind `10063`), caches the blob and returns it.
4. If no hinted server has it, the answer is `404`.

Expired entries are removed after `ALMOND_UPSTREAM_CACHE_TTL`; under capacity
pressure the oldest entries go first.
