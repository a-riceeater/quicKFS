// SPDX-License-Identifier: Apache-2.0
//! Background downloads and uploads with progress and cancellation.
//!
//! The app polls [`status`] rather than receiving callbacks, which keeps the
//! C ABI to plain request/response strings. Reads and writes are pipelined a
//! few chunks deep so a long-RTT link stays busy without buffering a whole
//! file in memory: peak buffered data is `PIPELINE_DEPTH * CHUNK_BYTES`.

use crate::{
    model::{BridgeError, BridgeResult, validate_name},
    session::{READ_WRITE, Session, lookup},
};
use futures::{StreamExt, stream::FuturesOrdered};
use quickfs_client_core::{
    ClientError, MAX_CLIENT_READ_SIZE, MAX_CLIENT_WRITE_SIZE, RemoteFilesystem,
};
use quickfs_protocol::{
    ErrorCode, FileAccess, FileHandle, FileOpenOptions, Name, NodeId, NodeKind,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

const CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const PIPELINE_DEPTH: usize = 8;
const MAX_TRACKED_TRANSFERS: usize = 1_024;

#[derive(Clone, Debug)]
enum State {
    Running,
    Finished,
    Failed(String, String),
    Cancelled,
}

#[derive(Default)]
pub struct Progress {
    bytes: AtomicU64,
    total: AtomicU64,
    files: AtomicU64,
    files_total: AtomicU64,
    cancel: AtomicBool,
    /// The file being copied now, relative to the transfer's root.
    current: Mutex<String>,
}

impl Progress {
    fn set_current(&self, name: impl Into<String>) {
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = name.into();
    }

    fn current(&self) -> String {
        self.current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn check(&self) -> BridgeResult<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(BridgeError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn add_bytes(&self, amount: u64) {
        self.bytes.fetch_add(amount, Ordering::Relaxed);
    }
}

struct Transfer {
    progress: Arc<Progress>,
    state: Mutex<State>,
}

static TRANSFERS: Mutex<Option<HashMap<u64, Arc<Transfer>>>> = Mutex::new(None);
static NEXT_TRANSFER: AtomicU64 = AtomicU64::new(1);

fn with_transfers<T>(body: impl FnOnce(&mut HashMap<u64, Arc<Transfer>>) -> T) -> T {
    let mut guard = TRANSFERS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    body(guard.get_or_insert_with(HashMap::new))
}

fn transfer(id: u64) -> BridgeResult<Arc<Transfer>> {
    with_transfers(|transfers| transfers.get(&id).cloned()).ok_or(BridgeError::UnknownTransfer)
}

/// Register a transfer and run `job` on the shared runtime.
pub fn start<F, Fut>(runtime: &tokio::runtime::Runtime, job: F) -> BridgeResult<Value>
where
    F: FnOnce(Arc<Progress>) -> Fut,
    Fut: std::future::Future<Output = BridgeResult<()>> + Send + 'static,
{
    let progress = Arc::new(Progress::default());
    let record = Arc::new(Transfer {
        progress: Arc::clone(&progress),
        state: Mutex::new(State::Running),
    });
    let id = NEXT_TRANSFER.fetch_add(1, Ordering::Relaxed);
    with_transfers(|transfers| {
        if transfers.len() >= MAX_TRACKED_TRANSFERS {
            transfers.retain(|_, transfer| matches!(lock_state(transfer), State::Running));
        }
        if transfers.len() >= MAX_TRACKED_TRANSFERS {
            return Err(BridgeError::invalid("too many transfers are running"));
        }
        transfers.insert(id, Arc::clone(&record));
        Ok(())
    })?;
    let future = job(progress);
    runtime.spawn(async move {
        let outcome = future.await;
        let mut state = record
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *state = match outcome {
            Ok(()) => State::Finished,
            Err(BridgeError::Cancelled) => State::Cancelled,
            Err(error) => State::Failed(error.code(), error.to_string()),
        };
    });
    Ok(json!({ "transfer": id }))
}

fn lock_state(transfer: &Transfer) -> State {
    transfer
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub fn status(id: u64) -> BridgeResult<Value> {
    let transfer = transfer(id)?;
    let progress = &transfer.progress;
    let mut reply = json!({
        "bytes": progress.bytes.load(Ordering::Relaxed),
        "total": progress.total.load(Ordering::Relaxed),
        "files": progress.files.load(Ordering::Relaxed),
        "files_total": progress.files_total.load(Ordering::Relaxed),
        "current": progress.current(),
    });
    let (state, error) = match lock_state(&transfer) {
        State::Running => ("running", None),
        State::Finished => ("finished", None),
        State::Cancelled => ("cancelled", None),
        State::Failed(code, message) => {
            ("failed", Some(json!({ "code": code, "message": message })))
        }
    };
    reply["state"] = json!(state);
    if let Some(error) = error {
        reply["error"] = error;
    }
    Ok(reply)
}

pub fn cancel(id: u64) -> BridgeResult<Value> {
    transfer(id)?.progress.cancel.store(true, Ordering::Release);
    Ok(json!({}))
}

pub fn forget(id: u64) -> Value {
    let removed = with_transfers(|transfers| match transfers.get(&id) {
        Some(transfer) if !matches!(lock_state(transfer), State::Running) => {
            transfers.remove(&id).is_some()
        }
        _ => false,
    });
    json!({ "forgotten": removed })
}

fn chunk_size(limit: u64, client_limit: u64) -> u64 {
    CHUNK_BYTES.min(limit.max(1)).min(client_limit)
}

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

enum Planned {
    Directory(PathBuf),
    File { node: NodeId, path: PathBuf },
    Symlink { node: NodeId, path: PathBuf },
}

/// Copy a remote file or directory tree to `destination`, which must not
/// exist unless `overwrite` is set (files only; directories merge).
pub async fn download(
    session: Arc<Session>,
    node: NodeId,
    destination: PathBuf,
    overwrite: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    let root = filesystem.get_metadata(node).await?;
    let destination_root = destination.clone();
    let mut plan = Vec::new();
    match root.kind {
        NodeKind::Directory => {
            let mut pending = vec![(node, destination.clone())];
            plan.push(Planned::Directory(destination));
            while let Some((directory, local)) = pending.pop() {
                progress.check()?;
                for entry in filesystem.list_directory(directory).await? {
                    let name = local_name(&entry.name)?;
                    let path = local.join(name);
                    match entry.kind {
                        NodeKind::Directory => {
                            plan.push(Planned::Directory(path.clone()));
                            pending.push((entry.node, path));
                        }
                        NodeKind::File => {
                            progress
                                .total
                                .fetch_add(entry.metadata.size, Ordering::Relaxed);
                            progress.files_total.fetch_add(1, Ordering::Relaxed);
                            plan.push(Planned::File {
                                node: entry.node,
                                path,
                            });
                        }
                        NodeKind::Symlink => plan.push(Planned::Symlink {
                            node: entry.node,
                            path,
                        }),
                        // Devices, sockets, and FIFOs have no portable local copy.
                        _ => {}
                    }
                }
            }
        }
        NodeKind::File => {
            progress.total.store(root.size, Ordering::Relaxed);
            progress.files_total.store(1, Ordering::Relaxed);
            plan.push(Planned::File {
                node,
                path: destination,
            });
        }
        NodeKind::Symlink => plan.push(Planned::Symlink {
            node,
            path: destination,
        }),
        _ => {
            return Err(BridgeError::invalid(
                "this kind of item cannot be downloaded",
            ));
        }
    }

    let read_chunk = chunk_size(session.capabilities.max_read_size, MAX_CLIENT_READ_SIZE);
    for step in plan {
        progress.check()?;
        match step {
            Planned::Directory(path) => match tokio::fs::create_dir(&path).await {
                Ok(()) => {}
                Err(error)
                    if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => {}
                Err(error) => return Err(error.into()),
            },
            Planned::Symlink { node, path } => {
                let target = filesystem.read_link(node).await?;
                create_local_symlink(&target, &path)?;
            }
            Planned::File { node, path } => {
                let shown = match path.strip_prefix(&destination_root) {
                    Ok(relative) if !relative.as_os_str().is_empty() => relative,
                    _ => path.file_name().map(Path::new).unwrap_or(&path),
                };
                progress.set_current(shown.display().to_string());
                download_file(filesystem, node, &path, overwrite, read_chunk, &progress).await?;
                progress.files.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

fn local_name(name: &Name) -> BridgeResult<&std::ffi::OsStr> {
    use std::os::unix::ffi::OsStrExt;
    validate_name(name.as_bytes())?;
    Ok(std::ffi::OsStr::from_bytes(name.as_bytes()))
}

fn create_local_symlink(target: &[u8], path: &Path) -> BridgeResult<()> {
    use std::os::unix::ffi::OsStrExt;
    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(target), path)?;
    Ok(())
}

async fn download_file(
    filesystem: &dyn RemoteFilesystem,
    node: NodeId,
    destination: &Path,
    overwrite: bool,
    chunk: u64,
    progress: &Progress,
) -> BridgeResult<()> {
    if !overwrite && tokio::fs::symlink_metadata(destination).await.is_ok() {
        return Err(BridgeError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("'{}' already exists", destination.display()),
        )));
    }
    let partial = partial_path(destination);
    let (handle, revision, size) = filesystem.open_file(node).await?;
    let copied = copy_to_local(
        filesystem, handle, revision, size, &partial, chunk, progress,
    )
    .await;
    let closed = filesystem.close_file(handle).await;
    if let Err(error) = copied.and(closed.map_err(Into::into)) {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&partial, destination).await {
        let _ = tokio::fs::remove_file(&partial).await;
        return Err(error.into());
    }
    Ok(())
}

fn partial_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    destination.with_file_name(format!(".{name}.{}.qkfs-partial", Uuid::new_v4().simple()))
}

async fn copy_to_local(
    filesystem: &dyn RemoteFilesystem,
    handle: FileHandle,
    revision: u64,
    size: u64,
    partial: &Path,
    chunk: u64,
    progress: &Progress,
) -> BridgeResult<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(partial)
        .await?;
    let mut offsets = (0..size).step_by(usize::try_from(chunk).unwrap_or(usize::MAX));
    let mut inflight = FuturesOrdered::new();
    loop {
        while inflight.len() < PIPELINE_DEPTH {
            let Some(offset) = offsets.next() else { break };
            let length = chunk.min(size - offset);
            inflight.push_back(async move {
                (
                    length,
                    filesystem
                        .read_range_versioned(handle, offset, length)
                        .await,
                )
            });
        }
        let Some((expected, read)) = inflight.next().await else {
            break;
        };
        progress.check()?;
        let read = read?;
        // A revision of zero means the implementation does not report one.
        if read.revision != 0 && revision != 0 && read.revision != revision {
            return Err(ClientError::StaleRevision.into());
        }
        file.write_all(&read.data).await?;
        progress.add_bytes(read.data.len() as u64);
        if (read.data.len() as u64) < expected {
            // The file shrank after open; what was read is the whole file.
            break;
        }
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload
// ---------------------------------------------------------------------------

/// Copy a local file or directory tree into `parent` as `name`. Existing
/// files are replaced only when `replace` is set; existing directories merge.
pub async fn upload(
    session: Arc<Session>,
    parent: NodeId,
    source: PathBuf,
    name: Name,
    replace: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    let write_chunk = chunk_size(session.capabilities.max_write_size, MAX_CLIENT_WRITE_SIZE);
    let metadata = tokio::fs::symlink_metadata(&source).await?;
    if metadata.is_file() {
        progress.total.store(metadata.len(), Ordering::Relaxed);
        progress.files_total.store(1, Ordering::Relaxed);
        progress.set_current(String::from_utf8_lossy(name.as_bytes()).into_owned());
        upload_file(
            filesystem,
            parent,
            &name,
            &source,
            replace,
            write_chunk,
            &progress,
        )
        .await?;
        progress.files.store(1, Ordering::Relaxed);
        let _ = filesystem.sync_directory(parent).await;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(BridgeError::invalid(
            "only files and folders can be uploaded",
        ));
    }

    // Size the whole tree first so progress is meaningful from the start.
    let mut files = Vec::new();
    let mut pending = vec![source.clone()];
    while let Some(directory) = pending.pop() {
        progress.check()?;
        let mut reader = tokio::fs::read_dir(&directory).await?;
        while let Some(entry) = reader.next_entry().await? {
            let kind = entry.file_type().await?;
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                let size = entry.metadata().await?.len();
                progress.total.fetch_add(size, Ordering::Relaxed);
                progress.files_total.fetch_add(1, Ordering::Relaxed);
                files.push(entry.path());
            }
        }
    }

    let root = ensure_directory(filesystem, parent, name, mode_of(&metadata)).await?;
    let mut directories: HashMap<PathBuf, NodeId> = HashMap::from([(source.clone(), root)]);
    files.sort();
    for file in files {
        progress.check()?;
        let relative = file
            .strip_prefix(&source)
            .map_err(|_| BridgeError::invalid("upload source changed during the walk"))?;
        progress.set_current(relative.display().to_string());
        let mut remote = root;
        let mut local = source.clone();
        if let Some(parent_path) = relative.parent() {
            for component in parent_path.components() {
                local.push(component);
                remote = match directories.get(&local) {
                    Some(node) => *node,
                    None => {
                        let mode = mode_of(&tokio::fs::metadata(&local).await?);
                        let node = ensure_directory(
                            filesystem,
                            remote,
                            os_name(component.as_os_str())?,
                            mode,
                        )
                        .await?;
                        directories.insert(local.clone(), node);
                        node
                    }
                };
            }
        }
        let file_name = os_name(
            file.file_name()
                .ok_or_else(|| BridgeError::invalid("upload file has no name"))?,
        )?;
        upload_file(
            filesystem,
            remote,
            &file_name,
            &file,
            replace,
            write_chunk,
            &progress,
        )
        .await?;
        progress.files.fetch_add(1, Ordering::Relaxed);
    }
    for node in directories.values() {
        let _ = filesystem.sync_directory(*node).await;
    }
    let _ = filesystem.sync_directory(parent).await;
    Ok(())
}

fn os_name(name: &std::ffi::OsStr) -> BridgeResult<Name> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes().to_vec();
    validate_name(&bytes)?;
    Ok(Name::new(bytes))
}

fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o7777
}

async fn ensure_directory(
    filesystem: &dyn RemoteFilesystem,
    parent: NodeId,
    name: Name,
    mode: u32,
) -> BridgeResult<NodeId> {
    match filesystem
        .create_directory(parent, name.clone(), mode | 0o700)
        .await
    {
        Ok(metadata) => Ok(metadata.node),
        Err(ClientError::Server(ErrorCode::AlreadyExists, _)) => {
            let node = lookup(filesystem, parent, &name).await?;
            if filesystem.get_metadata(node).await?.kind != NodeKind::Directory {
                return Err(ClientError::Server(
                    ErrorCode::NotDirectory,
                    String::from_utf8_lossy(name.as_bytes()).into_owned(),
                )
                .into());
            }
            Ok(node)
        }
        Err(error) => Err(error.into()),
    }
}

#[allow(clippy::too_many_arguments)]
async fn upload_file(
    filesystem: &dyn RemoteFilesystem,
    parent: NodeId,
    name: &Name,
    source: &Path,
    replace: bool,
    chunk: u64,
    progress: &Progress,
) -> BridgeResult<()> {
    let local = tokio::fs::File::open(source).await?;
    let mode = mode_of(&local.metadata().await?) & 0o777;
    let handle = match filesystem
        .create_file(parent, name.clone(), mode, READ_WRITE)
        .await
    {
        Ok(created) => created.opened.handle,
        Err(ClientError::Server(ErrorCode::AlreadyExists, _)) if replace => {
            let node = lookup(filesystem, parent, name).await?;
            filesystem
                .open_file_with_options(
                    node,
                    FileOpenOptions {
                        access: FileAccess::ReadWrite,
                        truncate: true,
                        append: false,
                    },
                )
                .await?
                .handle
        }
        Err(error) => return Err(error.into()),
    };
    let written = write_from_local(filesystem, handle, local, 0, chunk, progress).await;
    let synced = match &written {
        Ok(_) => filesystem
            .sync_file(handle, false)
            .await
            .map_err(BridgeError::from),
        Err(_) => Ok(()),
    };
    let closed = filesystem.close_file(handle).await;
    written?;
    synced?;
    closed?;
    Ok(())
}

/// Write a local file into an open remote handle starting at `offset`,
/// pipelining up to `PIPELINE_DEPTH` positioned writes, then flush.
pub async fn write_from_local(
    filesystem: &dyn RemoteFilesystem,
    handle: FileHandle,
    mut local: tokio::fs::File,
    offset: u64,
    chunk: u64,
    progress: &Progress,
) -> BridgeResult<u64> {
    let capacity = usize::try_from(chunk).unwrap_or(usize::MAX);
    let mut position = offset;
    let mut written = 0_u64;
    let mut exhausted = false;
    let mut inflight = FuturesOrdered::new();
    loop {
        while !exhausted && inflight.len() < PIPELINE_DEPTH {
            progress.check()?;
            let mut buffer = vec![0_u8; capacity];
            let mut filled = 0;
            while filled < capacity {
                let amount = local.read(&mut buffer[filled..]).await?;
                if amount == 0 {
                    exhausted = true;
                    break;
                }
                filled += amount;
            }
            if filled == 0 {
                break;
            }
            buffer.truncate(filled);
            let at = position;
            position = position
                .checked_add(filled as u64)
                .ok_or_else(|| BridgeError::invalid("write offset overflows"))?;
            inflight.push_back(async move {
                let result = filesystem.write_range(handle, at, &buffer).await;
                (buffer.len() as u64, result)
            });
        }
        let Some((expected, result)) = inflight.next().await else {
            break;
        };
        let result = result?;
        if result.written != expected {
            return Err(BridgeError::invalid("the server reported a partial write"));
        }
        written += result.written;
        progress.add_bytes(result.written);
    }
    filesystem.flush_file(handle, None).await?;
    Ok(written)
}

/// The CLI `write` command: overwrite part of an existing remote file with a
/// local file's bytes at `offset`.
pub async fn write_at(
    session: Arc<Session>,
    node: NodeId,
    source: PathBuf,
    offset: u64,
    sync: bool,
    progress: Arc<Progress>,
) -> BridgeResult<()> {
    let filesystem = session.filesystem.as_ref();
    if let Some(name) = source.file_name() {
        progress.set_current(name.to_string_lossy().into_owned());
    }
    let local = tokio::fs::File::open(&source).await?;
    progress
        .total
        .store(local.metadata().await?.len(), Ordering::Relaxed);
    progress.files_total.store(1, Ordering::Relaxed);
    let opened = filesystem.open_file_with_options(node, READ_WRITE).await?;
    let chunk = chunk_size(session.capabilities.max_write_size, MAX_CLIENT_WRITE_SIZE);
    let written =
        write_from_local(filesystem, opened.handle, local, offset, chunk, &progress).await;
    let synced = match (&written, sync) {
        (Ok(_), true) => filesystem
            .sync_file(opened.handle, false)
            .await
            .map_err(BridgeError::from),
        _ => Ok(()),
    };
    let closed = filesystem.close_file(opened.handle).await;
    written?;
    synced?;
    closed?;
    progress.files.store(1, Ordering::Relaxed);
    Ok(())
}
