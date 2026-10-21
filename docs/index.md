---
layout: home

hero:
  name: Almond
  text: Blossom media server for Nostr
  tagline: Temporary blob storage on your filesystem or S3 bucket. Authorized with Nostr signatures, expired automatically, cacheable from other Blossom servers.
  actions:
    - theme: brand
      text: Get started
      link: /guide/getting-started
    - theme: alt
      text: Configuration
      link: /configuration/
    - theme: alt
      text: Rust library
      link: /library
    - theme: alt
      text: GitHub
      link: https://github.com/flox1an/almond

features:
  - icon: 🌸
    title: Blossom API
    details: Upload (including resumable chunked uploads), download, mirror, list, delete and report. Implements BUD-01, BUD-02, BUD-04, BUD-06, BUD-07, BUD-09, BUD-11 and BUD-12.
  - icon: ⏳
    title: Temporary storage
    details: Filesystem only, no database. Age limits, size and count caps, oldest-first eviction. Optional S3-compatible bucket for uploads.
  - icon: 🔑
    title: Nostr access control
    details: Every write endpoint is off, public, wot (web of trust) or dvm, plus an optional npub allowlist. Optional Cashu payments per operation.
  - icon: 🌐
    title: Upstream caching
    details: Blobs missing locally are fetched from configured upstream servers or client hints, then proxied, redirected or cached.
  - icon: 🛠️
    title: Operations ready
    details: Prometheus metrics, BUD-11 hash filter, report quarantine, static file serving, config editor and built-in TLS.
  - icon: 🦀
    title: Embeddable
    details: A Rust library — merge the Blossom routes into your own Axum 0.8 application and keep your own homepage, auth and shutdown policy.
---
