//! Tail-follower stream for coalesced downstream downloads.
//!
//! Extracted from the 2100-line `handlers/upstream.rs`.  The stream reads from
//! a growing temp file that a background `run_download` task writes to, using
//! a `watch::Receiver` to block when data is not yet available.  No `AppState`
//! dependency — it is a pure transformation of `(File, Receiver, bounds)`.

use std::io::SeekFrom;

use futures_util::Stream;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use crate::models::{DownloadPhase, DownloadProgress};

/// Create a streaming response that reads from a growing file.
///
/// The stream starts at byte `start` (seeking into the file) and stops at
/// `end` (exclusive).  When `end` is `None` it follows the file until the
/// download completes.  The caller provides a `watch::Receiver` whose sender
/// is updated by `run_download` as new bytes are written and flushed.
pub async fn create_tailing_stream(
    mut reader: File,
    mut progress: watch::Receiver<DownloadProgress>,
    start: u64,
    end: Option<u64>,
) -> std::io::Result<impl Stream<Item = Result<bytes::Bytes, std::io::Error>>> {
    reader.seek(SeekFrom::Start(start)).await?;

    Ok(async_stream::try_stream! {
        let mut position = start;
        loop {
            let snapshot = *progress.borrow_and_update();
            let available = end.unwrap_or(u64::MAX).min(snapshot.written);

            if position < available {
                let to_read = std::cmp::min(64 * 1024, (available - position) as usize);
                let mut buffer = vec![0u8; to_read];
                reader.read_exact(&mut buffer).await?;
                position += to_read as u64;
                yield bytes::Bytes::from(buffer);
                continue;
            }

            if end.is_some_and(|limit| position >= limit) {
                break;
            }

            match snapshot.phase {
                DownloadPhase::Running => {
                    progress.changed().await.map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "download progress channel closed",
                        )
                    })?;
                }
                DownloadPhase::Complete => {
                    if end.is_some_and(|limit| position < limit) {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "download completed before the requested range",
                        ))?;
                    }
                    break;
                }
                DownloadPhase::Failed => {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "upstream download failed",
                    ))?;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{pin_mut, StreamExt};
    use tokio::io::AsyncWriteExt;

    async fn temp_download_file() -> (std::path::PathBuf, File, File) {
        let path = std::env::temp_dir().join(format!("almond-tail-{}", uuid::Uuid::new_v4()));
        let writer = File::create(&path).await.unwrap();
        let reader = File::open(&path).await.unwrap();
        (path, writer, reader)
    }

    #[tokio::test]
    async fn tailing_stream_joins_mid_download_and_terminates() {
        let (path, mut writer, reader) = temp_download_file().await;
        let (progress, receiver) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let stream = create_tailing_stream(reader, receiver, 0, None)
            .await
            .unwrap();
        pin_mut!(stream);

        writer.write_all(b"hello").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| state.written = 5);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"hello")
        );

        writer.write_all(b" world").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| {
            state.written = 11;
            state.phase = DownloadPhase::Complete;
        });
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b" world")
        );
        assert!(stream.next().await.is_none());
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_reports_download_failure() {
        let (path, mut writer, reader) = temp_download_file().await;
        let (progress, receiver) = watch::channel(DownloadProgress {
            written: 0,
            phase: DownloadPhase::Running,
        });
        let stream = create_tailing_stream(reader, receiver, 0, None)
            .await
            .unwrap();
        pin_mut!(stream);

        writer.write_all(b"abc").await.unwrap();
        writer.flush().await.unwrap();
        progress.send_modify(|state| state.written = 3);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"abc")
        );
        progress.send_modify(|state| state.phase = DownloadPhase::Failed);
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_respects_requested_bounds() {
        let (path, mut writer, reader) = temp_download_file().await;
        writer.write_all(b"abcdefgh").await.unwrap();
        writer.flush().await.unwrap();
        let (_, receiver) = watch::channel(DownloadProgress {
            written: 8,
            phase: DownloadPhase::Complete,
        });
        let stream = create_tailing_stream(reader, receiver, 2, Some(6))
            .await
            .unwrap();
        pin_mut!(stream);

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"cdef")
        );
        assert!(stream.next().await.is_none());
        tokio::fs::remove_file(path).await.unwrap();
    }

    #[tokio::test]
    async fn tailing_stream_rejects_truncated_completed_range() {
        let (path, mut writer, reader) = temp_download_file().await;
        writer.write_all(b"abc").await.unwrap();
        writer.flush().await.unwrap();
        let (_, receiver) = watch::channel(DownloadProgress {
            written: 3,
            phase: DownloadPhase::Complete,
        });
        let stream = create_tailing_stream(reader, receiver, 0, Some(5))
            .await
            .unwrap();
        pin_mut!(stream);

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            bytes::Bytes::from_static(b"abc")
        );
        let error = stream.next().await.unwrap().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
        tokio::fs::remove_file(path).await.unwrap();
    }
}