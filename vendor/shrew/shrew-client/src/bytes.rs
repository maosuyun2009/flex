//! Verified byte streams used by Rex file transfers.
//!
//! This is deliberately separate from the resumable terminal stream. A bytes
//! stream has no resume protocol: its empty binary frame and normal WebSocket
//! close are part of the transfer's success condition.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use shrew_proto::{BYTES_SUBPROTOCOL, data};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

use crate::{ClientError, Connection};

const CHUNK_SIZE: usize = 64 * 1024;
const IDLE_OPERATION: Duration = Duration::from_secs(30);
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Pin the selected object, rejecting symlinks/reparse points before any reservation.
pub fn open_upload_source(path: &Path) -> Result<std::fs::File, ClientError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(ClientError::Protocol(
            "upload source is not a regular file".into(),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            // FILE_ATTRIBUTE_REPARSE_POINT
            return Err(ClientError::Protocol(
                "upload source is a reparse point".into(),
            ));
        }
    }
    Ok(file)
}

pub async fn upload_file(
    connection: Connection,
    ticket: String,
    source: std::fs::File,
    expected_size: u64,
    cancel: Arc<AtomicBool>,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<(), ClientError> {
    let mut file = tokio::fs::File::from_std(source);
    let mut ws = open(&connection, &ticket, &cancel).await?;
    let mut sent = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE];

    while sent < expected_size {
        let wanted = (expected_size - sent).min(buf.len() as u64) as usize;
        let n = wait_io(&cancel, file.read(&mut buf[..wanted])).await?;
        if n == 0 {
            return Err(ClientError::Protocol(
                "source file ended before its declared size".into(),
            ));
        }
        wait(&cancel, async {
            ws.send(Message::binary(buf[..n].to_vec()))
                .await
                .map_err(ClientError::from)
        })
        .await?;
        sent += n as u64;
        progress(sent);
    }

    // A second read proves that a source which shrank or grew during transfer
    // was not silently accepted.
    let extra = wait_io(&cancel, file.read(&mut [0u8; 1])).await?;
    if extra != 0 {
        return Err(ClientError::Protocol(
            "source file grew during transfer".into(),
        ));
    }
    wait(&cancel, async {
        ws.send(Message::binary(Vec::new()))
            .await
            .map_err(ClientError::from)
    })
    .await?;

    loop {
        match next(&mut ws, &cancel).await? {
            Message::Close(frame) => {
                let code = frame.map(|f| u16::from(f.code));
                if code == Some(1000) {
                    return Ok(());
                }
                return Err(ClientError::Protocol(
                    "upload ended without a normal close".into(),
                ));
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Binary(_) | Message::Text(_) => {
                return Err(ClientError::Protocol(
                    "upload received data after half-close".into(),
                ));
            }
            Message::Frame(_) => {}
        }
    }
}

pub async fn download_file(
    connection: Connection,
    ticket: String,
    destination: PathBuf,
    overwrite: bool,
    cancel: Arc<AtomicBool>,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
) -> Result<u64, ClientError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let temp = NamedTempFile::new_in(parent).map_err(ClientError::Io)?;
    let temp_file = temp.reopen().map_err(ClientError::Io)?;
    let mut output = tokio::fs::File::from_std(temp_file);
    let mut ws = open(&connection, &ticket, &cancel).await?;
    let mut state = DownloadState::Receiving;
    let mut received = 0u64;

    loop {
        match next(&mut ws, &cancel).await? {
            Message::Binary(bytes) => {
                accept_download_frame(&mut state, &bytes)?;
                if !bytes.is_empty() {
                    wait_io(&cancel, output.write_all(&bytes)).await?;
                    received += bytes.len() as u64;
                    progress(received);
                }
            }
            Message::Close(frame) => {
                let code = frame.map(|f| u16::from(f.code));
                accept_download_close(&mut state, code)?;
                break;
            }
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Text(_) => {
                return Err(ClientError::Protocol("download sent a text frame".into()));
            }
            Message::Frame(_) => {}
        }
    }

    check_cancelled(&cancel)?;
    wait_io(&cancel, output.flush()).await?;
    wait_io(&cancel, output.sync_all()).await?;
    drop(output);
    check_cancelled(&cancel)?;
    let result = if overwrite {
        temp.persist(&destination)
    } else {
        temp.persist_noclobber(&destination)
    };
    result.map_err(|e| ClientError::Io(e.error))?;
    Ok(received)
}

async fn open(
    connection: &Connection,
    ticket: &str,
    cancel: &Arc<AtomicBool>,
) -> Result<crate::endpoint::Ws, ClientError> {
    let mut ws = wait(
        cancel,
        connection
            .endpoint()
            .websocket("/api/data", BYTES_SUBPROTOCOL),
    )
    .await?;
    let ticket = data::decode_ticket(ticket)?;
    wait(cancel, async {
        ws.send(Message::binary(data::hello(&ticket)))
            .await
            .map_err(ClientError::from)
    })
    .await?;
    loop {
        match next(&mut ws, cancel).await? {
            Message::Binary(frame) => {
                data::parse_reply(&frame)?;
                return Ok(ws);
            }
            Message::Close(_) => return Err(ClientError::Closed),
            Message::Ping(_) | Message::Pong(_) => {}
            Message::Text(_) | Message::Frame(_) => {
                return Err(ClientError::Protocol("malformed bytes welcome".into()));
            }
        }
    }
}

async fn next(
    ws: &mut crate::endpoint::Ws,
    cancel: &Arc<AtomicBool>,
) -> Result<Message, ClientError> {
    wait(cancel, async {
        ws.next()
            .await
            .transpose()
            .map_err(ClientError::from)?
            .ok_or(ClientError::Closed)
    })
    .await
}

async fn wait_io<T, F>(cancel: &Arc<AtomicBool>, future: F) -> Result<T, ClientError>
where
    F: std::future::Future<Output = std::io::Result<T>>,
{
    wait(cancel, async { future.await.map_err(ClientError::Io) }).await
}

async fn wait<T, F>(cancel: &Arc<AtomicBool>, future: F) -> Result<T, ClientError>
where
    F: std::future::Future<Output = Result<T, ClientError>>,
{
    if cancel.load(Ordering::Acquire) {
        return Err(ClientError::Cancelled);
    }
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => result,
        _ = cancelled(cancel) => Err(ClientError::Cancelled),
        _ = tokio::time::sleep(IDLE_OPERATION) => Err(ClientError::Timeout),
    }
}

async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), ClientError> {
    if cancel.load(Ordering::Acquire) {
        Err(ClientError::Cancelled)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadState {
    Receiving,
    HalfClosed,
    Complete,
}

fn accept_download_frame(state: &mut DownloadState, bytes: &[u8]) -> Result<(), ClientError> {
    match state {
        DownloadState::Receiving if bytes.is_empty() => {
            *state = DownloadState::HalfClosed;
            Ok(())
        }
        DownloadState::Receiving => Ok(()),
        DownloadState::HalfClosed | DownloadState::Complete => Err(ClientError::Protocol(
            "download sent data after half-close".into(),
        )),
    }
}

fn accept_download_close(state: &mut DownloadState, code: Option<u16>) -> Result<(), ClientError> {
    if *state == DownloadState::HalfClosed && code == Some(1000) {
        *state = DownloadState::Complete;
        Ok(())
    } else {
        Err(ClientError::Protocol(
            "download ended without empty frame and normal close".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opened_upload_source_stays_pinned_if_the_path_is_replaced() {
        use std::io::Read;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("selected.bin");
        std::fs::write(&path, b"original").unwrap();
        let mut pinned = open_upload_source(&path).unwrap();
        std::fs::rename(&path, root.path().join("moved.bin")).unwrap();
        std::fs::write(&path, b"replaced").unwrap();
        let mut bytes = Vec::new();
        pinned.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert!(open_upload_source(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn upload_source_rejects_symlinks_and_special_files() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("real.bin");
        std::fs::write(&target, b"private").unwrap();
        let link = root.path().join("link.bin");
        std::os::unix::fs::symlink(target, &link).unwrap();
        assert!(open_upload_source(&link).is_err());
        assert!(open_upload_source(Path::new("/dev/null")).is_err());
    }

    #[test]
    fn download_requires_empty_half_close_before_normal_close() {
        let mut state = DownloadState::Receiving;
        assert!(accept_download_frame(&mut state, &[]).is_ok());
        assert_eq!(state, DownloadState::HalfClosed);
        assert!(accept_download_frame(&mut state, b"late bytes").is_err());
        assert!(accept_download_close(&mut state, Some(1000)).is_ok());
    }

    #[test]
    fn early_or_abnormal_download_close_is_not_success() {
        let mut state = DownloadState::Receiving;
        assert!(accept_download_close(&mut state, Some(1000)).is_err());
        let mut state = DownloadState::HalfClosed;
        assert!(accept_download_close(&mut state, Some(1006)).is_err());
    }
}
