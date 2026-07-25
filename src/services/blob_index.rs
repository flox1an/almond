use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::models::FileMetadata;

/// Aggregates over the index, read without scanning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexStats {
    pub count: usize,
    pub total_bytes: u64,
    /// Bumped on every mutation. Lets callers cache derived artefacts (the
    /// BUD-11 filter, for instance) and detect staleness in O(1).
    pub generation: u64,
}

#[derive(Default)]
struct Entries {
    map: HashMap<String, Arc<FileMetadata>>,
    total_bytes: u64,
    generation: u64,
}

impl Entries {
    fn insert(&mut self, key: String, metadata: Arc<FileMetadata>) -> Option<Arc<FileMetadata>> {
        self.total_bytes += metadata.size;
        let previous = self.map.insert(key, metadata);
        if let Some(previous) = &previous {
            self.total_bytes = self.total_bytes.saturating_sub(previous.size);
        }
        self.generation += 1;
        previous
    }

    fn remove(&mut self, key: &str) -> Option<Arc<FileMetadata>> {
        let removed = self.map.remove(key);
        if let Some(removed) = &removed {
            self.total_bytes = self.total_bytes.saturating_sub(removed.size);
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
        let map: HashMap<String, Arc<FileMetadata>> = map
            .into_iter()
            .map(|(key, metadata)| {
                total_bytes += metadata.size;
                (key, Arc::new(metadata))
            })
            .collect();

        let mut entries = self.entries.write().await;
        entries.map = map;
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

    pub async fn remove(&self, sha256: &str) -> Option<Arc<FileMetadata>> {
        self.entries.write().await.remove(sha256)
    }

    /// Remove `sha256` only if its indexed path still matches `path`, guarding
    /// against evicting an entry that was re-uploaded while we were deleting.
    pub async fn remove_if_path_matches(&self, sha256: &str, path: &std::path::Path) -> bool {
        let mut entries = self.entries.write().await;
        let matches = entries
            .map
            .get(sha256)
            .map(|metadata| metadata.path == path)
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
            path: std::path::PathBuf::from(format!("/tmp/{}", size)),
            extension: None,
            mime_type: None,
            size,
            created_at: 0,
            pubkey: None,
            expiration: None,
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
    async fn remove_if_path_matches_guards_against_reupload() {
        let index = BlobIndex::new();
        index.insert("a".into(), meta(100)).await;

        assert!(!index.remove_if_path_matches("a", std::path::Path::new("/tmp/other")).await);
        assert_eq!(index.stats().await.count, 1);

        assert!(index.remove_if_path_matches("a", std::path::Path::new("/tmp/100")).await);
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
}
