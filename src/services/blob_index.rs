use std::cmp::Reverse;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::models::{BlobOrigin, FileMetadata};

/// Aggregates over the index, read without scanning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub count: usize,
    pub total_bytes: u64,
    /// Bumped on every mutation. Lets callers cache derived artefacts (the
    /// BUD-11 filter, for instance) and detect staleness in O(1).
    pub generation: u64,
}

/// Result of atomically publishing a completed blob.
pub enum PublishResult {
    /// The new metadata is now visible. Any entry it displaced is returned for
    /// physical cleanup by the storage layer.
    Published {
        displaced: Option<Arc<FileMetadata>>,
    },
    /// An existing entry retained precedence over this publication.
    Retained { existing: Arc<FileMetadata> },
}

#[derive(Default)]
struct Entries {
    map: HashMap<String, Arc<FileMetadata>>,
    total_bytes: u64,
    /// Newest-first `(created_at, sha256)` ordering.  This avoids cloning and
    /// sorting the entire catalogue for every `/list` page.
    order: BTreeSet<(Reverse<u64>, Reverse<String>)>,
    generation: u64,
}

impl Entries {
    fn insert(&mut self, key: String, metadata: Arc<FileMetadata>) -> Option<Arc<FileMetadata>> {
        self.total_bytes += metadata.size;
        let previous = self.map.insert(key.clone(), metadata.clone());
        if let Some(previous) = &previous {
            self.total_bytes = self.total_bytes.saturating_sub(previous.size);
            self.order
                .remove(&(Reverse(previous.created_at), Reverse(key.clone())));
        }
        self.order
            .insert((Reverse(metadata.created_at), Reverse(key)));
        self.generation += 1;
        previous
    }

    fn remove(&mut self, key: &str) -> Option<Arc<FileMetadata>> {
        let removed = self.map.remove(key);
        if let Some(removed) = &removed {
            self.total_bytes = self.total_bytes.saturating_sub(removed.size);
            self.order
                .remove(&(Reverse(removed.created_at), Reverse(key.to_owned())));
            self.generation += 1;
        }
        removed
    }
}

/// The in-memory catalogue of stored blobs.
///
/// Owning the map rather than exposing a bare `RwLock<HashMap<..>>` buys two
/// things the previous shape could not give:
///
/// * **Aggregates stay correct by construction.** `total_bytes` is maintained
///   on mutation instead of being recomputed by summing every entry on each
///   `/metrics` scrape.
/// * **`generation` makes derived artefacts cacheable.** `/filter` used to
///   rebuild a filter over every hash on every request; now it rebuilds only
///   when the generation moves.
///
/// Values are `Arc<FileMetadata>`, so a lookup on the hot serve path costs a
/// refcount bump rather than cloning a `PathBuf` and two `String`s.
#[derive(Default)]
pub struct BlobIndex {
    entries: RwLock<Entries>,
}

impl BlobIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the entire contents, e.g. after the startup filesystem scan.
    pub async fn replace(&self, map: HashMap<String, FileMetadata>) {
        let mut total_bytes = 0u64;
        let mut order = BTreeSet::new();
        let map: HashMap<String, Arc<FileMetadata>> = map
            .into_iter()
            .map(|(key, metadata)| {
                total_bytes += metadata.size;
                order.insert((Reverse(metadata.created_at), Reverse(key.clone())));
                (key, Arc::new(metadata))
            })
            .collect();

        let mut entries = self.entries.write().await;
        entries.map = map;
        entries.order = order;
        entries.total_bytes = total_bytes;
        entries.generation += 1;
    }

    pub async fn get(&self, sha256: &str) -> Option<Arc<FileMetadata>> {
        self.entries.read().await.map.get(sha256).cloned()
    }

    pub async fn contains(&self, sha256: &str) -> bool {
        self.entries.read().await.map.contains_key(sha256)
    }

    pub async fn insert(&self, key: String, metadata: FileMetadata) {
        self.entries.write().await.insert(key, Arc::new(metadata));
    }

    /// Publish one completed blob with origin-aware collision precedence.
    ///
    /// Uploads replace cache entries. Cache fills never replace an existing
    /// entry, including another cache fill. The returned displaced metadata is
    /// intentionally not deleted here: callers must first make the preferred
    /// index entry visible, then remove only the superseded physical copy.
    pub async fn publish(&self, key: String, metadata: FileMetadata) -> PublishResult {
        let mut entries = self.entries.write().await;
        if metadata.origin == BlobOrigin::UpstreamCache {
            if let Some(existing) = entries.map.get(&key) {
                return PublishResult::Retained {
                    existing: Arc::clone(existing),
                };
            }
        }

        PublishResult::Published {
            displaced: entries.insert(key, Arc::new(metadata)),
        }
    }

    pub async fn remove(&self, sha256: &str) -> Option<Arc<FileMetadata>> {
        self.entries.write().await.remove(sha256)
    }

    /// Remove `sha256` only if its indexed storage location still matches,
    /// guarding against evicting an entry that was re-published concurrently.
    pub async fn remove_if_location_matches(
        &self,
        sha256: &str,
        location: &crate::models::FileLocation,
    ) -> bool {
        let mut entries = self.entries.write().await;
        let matches = entries
            .map
            .get(sha256)
            .map(|metadata| metadata.location == *location)
            .unwrap_or(false);
        if matches {
            entries.remove(sha256);
        }
        matches
    }

    pub async fn stats(&self) -> IndexStats {
        let entries = self.entries.read().await;
        IndexStats {
            count: entries.map.len(),
            total_bytes: entries.total_bytes,
            generation: entries.generation,
        }
    }

    pub async fn generation(&self) -> u64 {
        self.entries.read().await.generation
    }

    /// Every entry, as cheap `Arc` clones. Callers that need to filter, sort or
    /// paginate should take a snapshot rather than hold the lock across `await`.
    pub async fn snapshot(&self) -> Vec<(String, Arc<FileMetadata>)> {
        let entries = self.entries.read().await;
        entries
            .map
            .iter()
            .map(|(key, metadata)| (key.clone(), Arc::clone(metadata)))
            .collect()
    }

    /// Return a newest-first page without allocating a full index snapshot.
    pub async fn page(
        &self,
        since: u64,
        until: u64,
        author: Option<&nostr_relay_pool::prelude::PublicKey>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Vec<(String, Arc<FileMetadata>)> {
        let entries = self.entries.read().await;
        let cursor_is_in_result = cursor
            .and_then(|sha256| entries.map.get(sha256).map(|metadata| (sha256, metadata)))
            .is_some_and(|(_, metadata)| {
                metadata.created_at >= since
                    && metadata.created_at <= until
                    && author.is_none_or(|author| metadata.pubkey.as_ref() == Some(author))
            });
        let mut after_cursor = !cursor_is_in_result;
        let mut page = Vec::with_capacity(limit);
        for (_, Reverse(sha256)) in &entries.order {
            if !after_cursor {
                if cursor.is_some_and(|cursor| cursor == sha256) {
                    after_cursor = true;
                }
                continue;
            }
            let Some(metadata) = entries.map.get(sha256) else {
                continue;
            };
            if metadata.created_at < since
                || metadata.created_at > until
                || author.is_some_and(|author| metadata.pubkey.as_ref() != Some(author))
            {
                continue;
            }
            page.push((sha256.clone(), Arc::clone(metadata)));
            if page.len() == limit {
                break;
            }
        }
        page
    }

    /// The well-formed 64-character SHA-256 keys, for filter construction.
    pub async fn hash_keys(&self) -> (Vec<String>, u64) {
        let entries = self.entries.read().await;
        let keys = entries
            .map
            .keys()
            .filter(|key| key.len() >= 64 && key[..64].bytes().all(|b| b.is_ascii_hexdigit()))
            .map(|key| key[..64].to_string())
            .collect();
        (keys, entries.generation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(size: u64) -> FileMetadata {
        FileMetadata {
            location: crate::models::FileLocation::Local(std::path::PathBuf::from(format!(
                "/tmp/{}",
                size
            ))),
            extension: None,
            mime_type: None,
            size,
            created_at: 0,
            pubkey: None,
            expiration: None,
            origin: BlobOrigin::Upload,
        }
    }

    #[tokio::test]
    async fn tracks_total_bytes_across_mutations() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;
        index.insert("b".into(), meta(250)).await;
        assert_eq!(index.stats().await.total_bytes, 350);
        assert_eq!(index.stats().await.count, 2);

        index.remove("a").await;
        assert_eq!(index.stats().await.total_bytes, 250);
        assert_eq!(index.stats().await.count, 1);
    }

    /// Re-inserting the same key must not double-count its bytes.
    #[tokio::test]
    async fn overwriting_a_key_replaces_its_size() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;
        index.insert("a".into(), meta(400)).await;
        assert_eq!(index.stats().await.count, 1);
        assert_eq!(index.stats().await.total_bytes, 400);
    }

    #[tokio::test]
    async fn removing_an_absent_key_is_inert() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;
        let before = index.stats().await;
        assert!(index.remove("missing").await.is_none());
        assert_eq!(index.stats().await, before);
    }

    #[tokio::test]
    async fn generation_advances_only_on_real_mutations() {
        let index = BlobIndex::new();
        let start = index.generation().await;
        index.insert("a".into(), meta(1)).await;
        let after_insert = index.generation().await;
        assert!(after_insert > start);

        assert!(index.get("a").await.is_some());
        assert_eq!(index.generation().await, after_insert);

        index.remove("nope").await;
        assert_eq!(index.generation().await, after_insert);

        index.remove("a").await;
        assert!(index.generation().await > after_insert);
    }

    #[tokio::test]
    async fn remove_if_location_matches_guards_against_reupload() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;

        assert!(
            !index
                .remove_if_location_matches(
                    "a",
                    &crate::models::FileLocation::Local(std::path::PathBuf::from("/tmp/other")),
                )
                .await
        );
        assert_eq!(index.stats().await.count, 1);

        assert!(
            index
                .remove_if_location_matches(
                    "a",
                    &crate::models::FileLocation::Local(std::path::PathBuf::from("/tmp/100")),
                )
                .await
        );
        assert_eq!(index.stats().await.count, 0);
    }

    #[tokio::test]
    async fn replace_recomputes_aggregates() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;

        let mut map = HashMap::new();
        map.insert("x".to_string(), meta(7));
        map.insert("y".to_string(), meta(8));
        index.replace(map).await;

        let stats = index.stats().await;
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_bytes, 15);
        assert!(index.get("a").await.is_none());
    }

    #[tokio::test]
    async fn hash_keys_skips_non_sha256_entries() {
        let index = BlobIndex::new();
        let sha = "a".repeat(64);
        index.insert(sha.clone(), meta(1)).await;
        index.insert("short".into(), meta(1)).await;
        index.insert("z".repeat(64), meta(1)).await; // not hex

        let (keys, _) = index.hash_keys().await;
        assert_eq!(keys, vec![sha]);
    }

    #[tokio::test]
    async fn page_is_newest_first_without_snapshot_sorting() {
        let index = BlobIndex::new();
        let mut oldest = meta(1);
        oldest.created_at = 1;
        let mut newest = meta(1);
        newest.created_at = 3;
        let mut middle = meta(1);
        middle.created_at = 2;
        index.insert("a".into(), oldest).await;
        index.insert("c".into(), newest).await;
        index.insert("b".into(), middle).await;

        let first = index.page(0, u64::MAX, None, None, 1).await;
        assert_eq!(first[0].0, "c");
        let second = index.page(0, u64::MAX, None, Some("c"), 2).await;
        assert_eq!(
            second
                .into_iter()
                .map(|(sha256, _)| sha256)
                .collect::<Vec<_>>(),
            vec!["b", "a"]
        );
    }
    #[tokio::test]
    async fn upload_publication_replaces_cached_metadata() {
        let index = BlobIndex::new();
        let mut cached = meta(10);
        cached.origin = BlobOrigin::UpstreamCache;
        assert!(matches!(
            index.publish("hash".into(), cached).await,
            PublishResult::Published { displaced: None }
        ));

        assert!(matches!(
            index.publish("hash".into(), meta(20)).await,
            PublishResult::Published { displaced: Some(_) }
        ));
        let metadata = index.get("hash").await.unwrap();
        assert_eq!(metadata.origin, BlobOrigin::Upload);
        assert_eq!(metadata.size, 20);
    }

    #[tokio::test]
    async fn cache_publication_retains_existing_upload() {
        let index = BlobIndex::new();
        index.publish("hash".into(), meta(20)).await;
        let mut cached = meta(10);
        cached.origin = BlobOrigin::UpstreamCache;

        assert!(matches!(
            index.publish("hash".into(), cached).await,
            PublishResult::Retained { .. }
        ));
        assert_eq!(index.get("hash").await.unwrap().origin, BlobOrigin::Upload);
    }
}
