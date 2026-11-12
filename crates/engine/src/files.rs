use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use shrew_client::{ClientError, Connection};
use shrew_core::{CoreError, Shrew};

const PROGRESS_INTERVAL: Duration = Duration::from_millis(50);
const CANCEL_POLL: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTarget {
    pub server_id: String,
    pub address: String,
    pub session_id: String,
    pub block_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgress {
    pub completed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub file_name: String,
}

type Progress = Arc<dyn Fn(TransferProgress) + Send + Sync>;

type LocalFile = (std::fs::File, String, u64);

#[derive(Debug, Clone)]
pub struct FileCapabilities {
    pub revision: u32,
    pub upload: bool,
    pub download: bool,
}

pub(crate) async fn capabilities(
    connection: Connection,
    target: FileTarget,
) -> Result<FileCapabilities, CoreError> {
    let revision = connection.welcome().api_revision;
    let description = call(
        &connection,
        "session.describe_block",
        json!({"session_id":target.session_id,"block_id":target.block_id}),
        &Arc::new(AtomicBool::new(false)),
    )
    .await?;
    let methods = description.pointer("/api/methods");
    Ok(FileCapabilities {
        revision,
        upload: revision >= 11 && methods.is_some_and(|m| m.get("upload_begin").is_some()),
        download: revision >= 13 && methods.is_some_and(|m| m.get("download_begin").is_some()),
    })
}

pub(crate) fn connection_for_target(
    shrew: &Shrew,
    target: &FileTarget,
) -> Result<Connection, CoreError> {
    if target.server_id.is_empty()
        || target.address.is_empty()
        || target.session_id.is_empty()
        || target.block_id.is_empty()
    {
        return Err(CoreError::Server("Invalid file transfer target".into()));
    }
    let state = shrew.state();
    let server = state
        .servers
        .iter()
        .find(|server| server.id == target.server_id)
        .ok_or(CoreError::UnknownServer)?;
    if server.address != target.address {
        return Err(CoreError::Server("Saved server address changed".into()));
    }
    shrew.persistent_connection(&target.server_id)
}

pub(crate) async fn upload_with_connection(
    connection: Connection,
    target: FileTarget,
    paths: Vec<PathBuf>,
    cancel: Arc<AtomicBool>,
    progress: Progress,
) -> Result<(), CoreError> {
    let files = validate_uploads(&paths)?;
    check_cancelled(&cancel)?;
    require_method(&connection, &target, &cancel, "upload_begin", 11).await?;
    let batch = format!("upload:{}", uuid::Uuid::new_v4());

    for (index, (file, name, size)) in files.into_iter().enumerate() {
        check_cancelled(&cancel)?;
        let reply = call(
            &connection,
            "com.superlogical.terminal.upload_begin",
            json!({
                "session_id": target.session_id.clone(),
                "block_id": target.block_id.clone(),
                "args": {
                    "ref": format!("upload:{}", uuid::Uuid::new_v4()),
                    "name": name.clone(),
                    "size": size,
                    "batch": batch.clone(),
                    "index": index,
                    "count": paths.len(),
                }
            }),
            &cancel,
        )
        .await?;
        let ticket = reply
            .get("ticket")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::Server("Upload reservation did not return a ticket".into()))?
            .to_owned();
        let file_progress = throttled_progress(progress.clone(), name.clone(), Some(size));
        shrew_client::upload_file(
            connection.clone(),
            ticket,
            file,
            size,
            cancel.clone(),
            file_progress,
        )
        .await
        .map_err(client_error)?;
        emit_progress(&progress, size, Some(size), name);
    }
    Ok(())
}

pub(crate) async fn download_with_connection(
    connection: Connection,
    target: FileTarget,
    remote_path: String,
    destination: PathBuf,
    overwrite: bool,
    cancel: Arc<AtomicBool>,
    progress: Progress,
) -> Result<(), CoreError> {
    validate_destination(&destination, overwrite)?;
    if remote_path.is_empty() {
        return Err(CoreError::Server("Remote path is empty".into()));
    }
    check_cancelled(&cancel)?;
    require_method(&connection, &target, &cancel, "download_begin", 13).await?;
    let name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download")
        .to_owned();
    let reply = call(
        &connection,
        "com.superlogical.terminal.download_begin",
        json!({
            "session_id": target.session_id.clone(),
            "block_id": target.block_id.clone(),
            "args": {
                "ref": format!("download:{}", uuid::Uuid::new_v4()),
                "path": remote_path,
            }
        }),
        &cancel,
    )
    .await?;
    let ticket = reply
        .get("ticket")
        .and_then(Value::as_str)
        .ok_or_else(|| CoreError::Server("Download reservation did not return a ticket".into()))?
        .to_owned();
    let total = reply.get("size").and_then(Value::as_u64);
    let file_progress = throttled_progress(progress.clone(), name.clone(), total);
    shrew_client::download_file(
        connection,
        ticket,
        destination,
        overwrite,
        cancel,
        file_progress,
    )
    .await
    .map(|bytes| emit_progress(&progress, bytes, total, name))
    .map_err(client_error)
}

async fn require_method(
    connection: &Connection,
    target: &FileTarget,
    cancel: &Arc<AtomicBool>,
    method: &str,
    minimum: u32,
) -> Result<(), CoreError> {
    if connection.welcome().api_revision < minimum {
        return Err(CoreError::Server(format!(
            "{method} needs API {minimum}; this connection negotiated {}",
            connection.welcome().api_revision
        )));
    }
    let description = call(
        connection,
        "session.describe_block",
        json!({"session_id": target.session_id.clone(), "block_id": target.block_id.clone()}),
        cancel,
    )
    .await?;
    if description
        .pointer("/api/methods")
        .is_some_and(|methods| methods.get(method).is_some())
    {
        Ok(())
    } else {
        Err(CoreError::Server(format!(
            "This block does not support {method}"
        )))
    }
}

async fn call(
    connection: &Connection,
    method: &str,
    payload: Value,
    cancel: &Arc<AtomicBool>,
) -> Result<Value, CoreError> {
    let reply = connection.start(method, payload).map_err(client_error)?;
    tokio::select! {
        result = reply => result.map(|(_, value)| value).map_err(client_error),
        _ = cancelled(cancel) => Err(cancelled_error()),
    }
}

fn validate_uploads(paths: &[PathBuf]) -> Result<Vec<LocalFile>, CoreError> {
    if paths.is_empty() {
        return Err(CoreError::Server("No files selected".into()));
    }
    if paths.len() > 256 {
        return Err(CoreError::Server(
            "Choose at most 256 files per batch".into(),
        ));
    }
    paths
        .iter()
        .map(|path| {
            let file = shrew_client::open_upload_source(path).map_err(client_error)?;
            let metadata = file
                .metadata()
                .map_err(|e| CoreError::Server(e.to_string()))?;
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| {
                    !name.is_empty()
                        && *name != "."
                        && *name != ".."
                        && !name.chars().any(char::is_control)
                })
                .ok_or_else(|| CoreError::Server(format!("Invalid file name: {}", path.display())))?
                .to_owned();
            Ok((file, name, metadata.len()))
        })
        .collect()
}

fn validate_destination(destination: &Path, overwrite: bool) -> Result<(), CoreError> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    let parent_metadata = std::fs::metadata(parent)
        .map_err(|e| CoreError::Server(format!("Cannot access destination folder: {e}")))?;
    if !parent_metadata.is_dir() {
        return Err(CoreError::Server(
            "Destination parent is not a directory".into(),
        ));
    }
    match std::fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.is_dir() => {
            Err(CoreError::Server("Destination is a directory".into()))
        }
        Ok(metadata) if !metadata.file_type().is_file() => Err(CoreError::Server(
            "Destination is not a regular file".into(),
        )),
        Ok(_) if !overwrite => Err(CoreError::Server("Destination already exists".into())),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(CoreError::Server(format!("Cannot access destination: {e}"))),
    }
}

fn throttled_progress(
    progress: Progress,
    file_name: String,
    total: Option<u64>,
) -> Arc<dyn Fn(u64) + Send + Sync> {
    let last = Arc::new(Mutex::new(Instant::now() - PROGRESS_INTERVAL));
    Arc::new(move |completed| {
        let mut last_sent = last.lock().unwrap();
        if completed == total.unwrap_or(u64::MAX) || last_sent.elapsed() >= PROGRESS_INTERVAL {
            *last_sent = Instant::now();
            progress(TransferProgress {
                completed_bytes: completed,
                total_bytes: total,
                file_name: file_name.clone(),
            });
        }
    })
}

fn emit_progress(progress: &Progress, completed: u64, total: Option<u64>, file_name: String) {
    progress(TransferProgress {
        completed_bytes: completed,
        total_bytes: total,
        file_name,
    });
}

fn client_error(error: ClientError) -> CoreError {
    if matches!(error, ClientError::Cancelled) {
        cancelled_error()
    } else {
        CoreError::from(error)
    }
}

fn cancelled_error() -> CoreError {
    CoreError::Server("Cancelled".into())
}

fn check_cancelled(cancel: &AtomicBool) -> Result<(), CoreError> {
    if cancel.load(Ordering::Acquire) {
        Err(cancelled_error())
    } else {
        Ok(())
    }
}

async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}
