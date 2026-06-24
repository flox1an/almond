use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
};

use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::RwLock,
};
use tracing::{info, warn};

use crate::models::ServeFileMetadata;

const HASH_READ_BUFFER_SIZE: usize = 64 * 1024;

pub async fn refresh_serve_file_index(
    root: &Path,
    manifest_name: &str,
    index: &RwLock<HashMap<String, ServeFileMetadata>>,
) -> std::io::Result<()> {
    let root = root.to_path_buf();
    let files = collect_files(&root, manifest_name).await?;
    let mut entries = Vec::with_capacity(files.len());
    let mut next_index = HashMap::with_capacity(files.len());

    for path in files {
        let sha256 = sha256_file(&path).await?;
        let metadata = fs::metadata(&path).await?;
        let extension = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_string());
        let mime_type = mime_guess::from_path(&path)
            .first()
            .map(|mime| mime.essence_str().to_string());
        let manifest_path = manifest_relative_path(&root, &path);

        entries.push((sha256.clone(), manifest_path));
        next_index.insert(
            sha256,
            ServeFileMetadata {
                path,
                extension,
                mime_type,
                size: metadata.len(),
            },
        );
    }

    entries.sort_by(|a, b| a.1.cmp(&b.1));
    if let Err(e) = write_manifest(&root.join(manifest_name), &entries).await {
        warn!(
            "⚠️ Failed to write serve files manifest {}: {}",
            root.join(manifest_name).display(),
            e
        );
    }

    let indexed_count = next_index.len();
    *index.write().await = next_index;
    info!(
        "📁 Serve files index refreshed: {} files from {}",
        indexed_count,
        root.display()
    );

    Ok(())
}

pub async fn get_serve_file(
    index: &RwLock<HashMap<String, ServeFileMetadata>>,
    sha256: &str,
) -> Option<ServeFileMetadata> {
    index.read().await.get(sha256).cloned()
}

async fn collect_files(root: &Path, manifest_name: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs_to_process = vec![root.to_path_buf()];

    while let Some(dir) = dirs_to_process.pop() {
        let mut entries = fs::read_dir(&dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let name = entry.file_name();
            let file_type = entry.file_type().await?;

            if file_type.is_dir() {
                if name.to_str() != Some(".git") {
                    dirs_to_process.push(path);
                }
            } else if file_type.is_file() && name.to_str() != Some(manifest_name) {
                files.push(path);
            }
        }
    }

    Ok(files)
}

async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_READ_BUFFER_SIZE];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(hex::encode(hasher.finalize()))
}

fn manifest_relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut parts = Vec::new();

    for component in relative.components() {
        if let Component::Normal(part) = component {
            parts.push(part.to_string_lossy());
        }
    }

    format!("./{}", parts.join("/"))
}

async fn write_manifest(manifest_path: &Path, entries: &[(String, String)]) -> std::io::Result<()> {
    let mut file = fs::File::create(manifest_path).await?;

    for (sha256, path) in entries {
        file.write_all(format!("{}  {}\n", sha256, path).as_bytes())
            .await?;
    }

    Ok(())
}

pub fn start_refresh_job(
    root: PathBuf,
    manifest_name: String,
    refresh_interval_secs: u64,
    index: std::sync::Arc<RwLock<HashMap<String, ServeFileMetadata>>>,
) {
    if refresh_interval_secs == 0 {
        info!("📁 Serve files periodic refresh disabled");
        return;
    }

    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(tokio::time::Duration::from_secs(refresh_interval_secs));
        interval.tick().await;

        loop {
            interval.tick().await;
            if let Err(e) = refresh_serve_file_index(&root, &manifest_name, &index).await {
                warn!(
                    "⚠️ Failed to refresh serve files index for {}: {}",
                    root.display(),
                    e
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, fs, sync::Arc};

    use tokio::sync::RwLock;
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn refresh_serve_file_index_writes_shasum_compatible_manifest() {
        let root = std::env::temp_dir().join(format!("almond-serve-files-{}", Uuid::new_v4()));
        let nested = root.join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("hello.txt"), b"hello").unwrap();
        fs::write(nested.join("world.txt"), b"world").unwrap();

        let index = Arc::new(RwLock::new(HashMap::new()));
        refresh_serve_file_index(&root, "manifest-sha256.txt", &index)
            .await
            .unwrap();

        let manifest = fs::read_to_string(root.join("manifest-sha256.txt")).unwrap();
        assert!(manifest.contains("  ./hello.txt\n"));
        assert!(manifest.contains("  ./nested/world.txt\n"));
        assert_eq!(index.read().await.len(), 2);

        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn refresh_serve_file_index_skips_manifest_and_git_directory() {
        let root = std::env::temp_dir().join(format!("almond-serve-files-{}", Uuid::new_v4()));
        let git = root.join(".git");
        fs::create_dir_all(&git).unwrap();
        fs::write(root.join("manifest-sha256.txt"), b"ignored").unwrap();
        fs::write(root.join("served.txt"), b"served").unwrap();
        fs::write(git.join("secret"), b"ignored").unwrap();

        let index = Arc::new(RwLock::new(HashMap::new()));
        refresh_serve_file_index(&root, "manifest-sha256.txt", &index)
            .await
            .unwrap();

        let manifest = fs::read_to_string(root.join("manifest-sha256.txt")).unwrap();
        assert!(manifest.contains("  ./served.txt\n"));
        assert!(!manifest.contains("manifest-sha256.txt"));
        assert!(!manifest.contains(".git"));
        assert_eq!(index.read().await.len(), 1);

        fs::remove_dir_all(root).unwrap();
    }
}
