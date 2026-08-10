use std::{
    env,
    path::Path,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use aws_config::{BehaviorVersion, Region};
use aws_credential_types::{provider::SharedCredentialsProvider, Credentials};
use aws_sdk_s3::{config::Builder as S3ConfigBuilder, primitives::ByteStream, Client};
use tokio::fs;

use crate::{
    error::{AppError, AppResult},
    models::{BlobOrigin, FileLocation, FileMetadata},
    services::blob_name,
};

const REQUIRED_S3_ENV: [&str; 4] = [
    "ALMOND_S3_ENDPOINT",
    "ALMOND_S3_BUCKET",
    "ALMOND_S3_ACCESS_KEY_ID",
    "ALMOND_S3_SECRET_ACCESS_KEY",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct S3Settings {
    pub endpoint: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
}

impl S3Settings {
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_values(
            REQUIRED_S3_ENV
                .map(|name| env::var(name).ok().filter(|value| !value.trim().is_empty())),
        )
    }

    fn from_values(values: [Option<String>; 4]) -> Result<Option<Self>, String> {
        if values.iter().all(Option::is_none) {
            return Ok(None);
        }
        let missing: Vec<&str> = REQUIRED_S3_ENV
            .iter()
            .zip(&values)
            .filter_map(|(name, value)| value.is_none().then_some(*name))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Incomplete S3 configuration; missing: {}",
                missing.join(", ")
            ));
        }
        Ok(Some(Self {
            endpoint: values[0].clone().expect("validated"),
            bucket: values[1].clone().expect("validated"),
            access_key_id: values[2].clone().expect("validated"),
            secret_access_key: values[3].clone().expect("validated"),
        }))
    }
}
pub struct NativeS3Storage {
    client: Client,
    bucket: String,
}

impl NativeS3Storage {
    pub async fn connect(settings: S3Settings) -> Self {
        let credentials = SharedCredentialsProvider::new(Credentials::new(
            settings.access_key_id,
            settings.secret_access_key,
            None,
            None,
            "almond",
        ));
        let shared_config = aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new("auto"))
            .credentials_provider(credentials)
            .endpoint_url(settings.endpoint)
            .load()
            .await;
        let config = S3ConfigBuilder::from(&shared_config)
            .force_path_style(false)
            .build();
        Self {
            client: Client::from_conf(config),
            bucket: settings.bucket,
        }
    }

    pub async fn put(
        &self,
        temp_path: &Path,
        sha256: &str,
        extension: Option<&str>,
        expiration: Option<u64>,
    ) -> AppResult<String> {
        let key = object_key(sha256, extension, expiration)?;
        let body = ByteStream::from_path(temp_path).await.map_err(|error| {
            AppError::IoError(format!(
                "Unable to read temporary blob for S3 upload: {error}"
            ))
        })?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                AppError::ServiceUnavailable(format!("S3 PutObject failed: {error}"))
            })?;
        fs::remove_file(temp_path).await?;
        Ok(key)
    }

    pub async fn find(&self, sha256: &str) -> AppResult<Option<FileMetadata>> {
        let prefix = hash_prefix(sha256)?;
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.try_next().await.map_err(|error| {
            AppError::ServiceUnavailable(format!("S3 ListObjectsV2 failed: {error}"))
        })? {
            for object in page.contents() {
                let Some(key) = object.key() else {
                    continue;
                };
                let Some((hash, expiration, extension)) = parse_object_key(key) else {
                    continue;
                };
                if hash != sha256 {
                    continue;
                }
                return Ok(Some(FileMetadata {
                    location: FileLocation::S3 {
                        key: key.to_owned(),
                    },
                    extension: extension.clone(),
                    mime_type: extension.as_deref().and_then(|ext| {
                        mime_guess::from_ext(ext)
                            .first()
                            .map(|mime| mime.essence_str().to_owned())
                    }),
                    size: object.size().unwrap_or_default().max(0) as u64,
                    created_at: object
                        .last_modified()
                        .map_or_else(now_secs, |time| time.secs().max(0) as u64),
                    pubkey: None,
                    expiration,
                    origin: BlobOrigin::Upload,
                }));
            }
        }
        Ok(None)
    }

    pub async fn get(
        &self,
        key: &str,
        range: Option<&str>,
    ) -> AppResult<aws_sdk_s3::operation::get_object::GetObjectOutput> {
        let mut request = self.client.get_object().bucket(&self.bucket).key(key);
        if let Some(range) = range {
            request = request.range(range);
        }
        request
            .send()
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("S3 GetObject failed: {error}")))
    }
    pub async fn read_text(&self, key: &str) -> AppResult<String> {
        let output = self.get(key, None).await?;
        let bytes = output
            .body
            .collect()
            .await
            .map_err(|error| AppError::ServiceUnavailable(format!("S3 body read failed: {error}")))?
            .into_bytes();
        String::from_utf8(bytes.to_vec())
            .map_err(|error| AppError::IoError(format!("S3 object was not UTF-8: {error}")))
    }

    pub async fn delete(&self, key: &str) -> AppResult<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|error| {
                AppError::ServiceUnavailable(format!("S3 DeleteObject failed: {error}"))
            })?;
        Ok(())
    }

    pub async fn delete_matching(&self, sha256: &str) -> AppResult<()> {
        let prefix = hash_prefix(sha256)?;
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.try_next().await.map_err(|error| {
            AppError::ServiceUnavailable(format!("S3 ListObjectsV2 failed: {error}"))
        })? {
            for object in page.contents() {
                let Some(key) = object.key() else {
                    continue;
                };
                if parse_object_key(key).is_some_and(|(hash, _, _)| hash == sha256) {
                    self.client
                        .delete_object()
                        .bucket(&self.bucket)
                        .key(key)
                        .send()
                        .await
                        .map_err(|error| {
                            AppError::ServiceUnavailable(format!("S3 DeleteObject failed: {error}"))
                        })?;
                }
            }
        }
        Ok(())
    }

    pub async fn list_all(&self) -> AppResult<Vec<(String, FileMetadata)>> {
        let mut entries = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .into_paginator()
            .send();
        while let Some(page) = pages.try_next().await.map_err(|error| {
            AppError::ServiceUnavailable(format!("S3 ListObjectsV2 failed: {error}"))
        })? {
            for object in page.contents() {
                let Some(key) = object.key() else {
                    continue;
                };
                let Some((hash, expiration, extension)) = parse_object_key(key) else {
                    continue;
                };
                entries.push((
                    hash,
                    FileMetadata {
                        location: FileLocation::S3 {
                            key: key.to_owned(),
                        },
                        extension: extension.clone(),
                        mime_type: extension.as_deref().and_then(|ext| {
                            mime_guess::from_ext(ext)
                                .first()
                                .map(|mime| mime.essence_str().to_owned())
                        }),
                        size: object.size().unwrap_or_default().max(0) as u64,
                        created_at: object
                            .last_modified()
                            .map_or_else(now_secs, |time| time.secs().max(0) as u64),
                        pubkey: None,
                        expiration,
                        origin: BlobOrigin::Upload,
                    },
                ));
            }
        }
        Ok(entries)
    }
}

pub type SharedNativeS3Storage = Arc<NativeS3Storage>;

pub fn object_key(
    sha256: &str,
    extension: Option<&str>,
    expiration: Option<u64>,
) -> AppResult<String> {
    let (h0, h1) = blob_name::fan_out(sha256)?;
    let filename = blob_name::name(sha256, expiration, extension)?;
    Ok(format!("{h0}/{h1}/{filename}"))
}

fn hash_prefix(sha256: &str) -> AppResult<String> {
    let (h0, h1) = blob_name::fan_out(sha256)?;
    Ok(format!("{h0}/{h1}/{sha256}"))
}

fn parse_object_key(key: &str) -> Option<(String, Option<u64>, Option<String>)> {
    let mut path = key.split('/');
    let (first, second, filename) = (path.next()?, path.next()?, path.next()?);
    if path.next().is_some() {
        return None;
    }
    let parsed = blob_name::parse(filename)?;
    (first == &parsed.hash[..1] && second == &parsed.hash[1..2])
        .then(|| (parsed.hash, parsed.expiration, parsed.extension))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn s3_configuration_requires_all_values() {
        let error = S3Settings::from_values([
            Some("endpoint".to_owned()),
            Some("bucket".to_owned()),
            None,
            None,
        ])
        .unwrap_err();
        assert_eq!(
            error,
            "Incomplete S3 configuration; missing: ALMOND_S3_ACCESS_KEY_ID, ALMOND_S3_SECRET_ACCESS_KEY"
        );
    }
    #[test]
    fn object_keys_follow_native_layout() {
        let hash = "aabb".repeat(16);
        assert_eq!(
            object_key(&hash, Some("jpg"), Some(42)).unwrap(),
            format!("a/a/{hash}_42.jpg")
        );
    }
}
