# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Almond operators and trusted collaborators administer one server instance.

## Product Purpose

Almond is a Blossom media server for Nostr. Its administrative interface supports inspecting and moderating blobs and reports, managing access and configuration, monitoring operations, and administering the optional Cashu wallet.

## Operating Context

Administrators use Nostr signers rather than passwords. Deployments may be externally managed or opt into browser-managed settings. CLI arguments, environment variables, and existing dotenv files remain authoritative over managed settings.

## Capabilities and Constraints

The admin interface must not accept private keys or introduce an administration database, role framework, generic hot reload, or remote host management. Root administrator keys are deployment-managed; collaborators have full administrative access. The interface must remain usable on desktop and mobile and provide accessible controls and clear operational states. The existing wallet bearer interface will be replaced with NIP-98 authorization as a documented breaking cutover. S3 quarantine must be reversible. Moderation takes precedence over paid retention without automatic refunds.

## Brand Commitments

The existing Almond interface and assets are evidence for its identity.

## Evidence on Hand

`docs/admin-concept.md` describes the agreed admin workflows, security constraints, acceptance criteria, and remaining implementation decisions. `src/config-editor.html` is the existing configuration editor.
