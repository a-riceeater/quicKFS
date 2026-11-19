// SPDX-License-Identifier: Apache-2.0
//! The callbacks for an open named stream — an extended attribute presented as
//! `file:name`. See [`crate::streams`] for the mapping itself.
//!
//! An attribute is a value, not a seekable file, so an open stream holds the
//! value in memory: it is read from the server the first time it is needed,
//! edited in place by writes and size changes, and written back whole on flush
//! and on the last close. Values are bounded by the adapter's attribute limit,
//! which is what keeps that copy affordable.

use super::{FileContext, Location, WindowsFilesystem, adapter_status};
use crate::{
    handle::Handle,
    resolve::Resolved,
    status::{self, NtStatus},
    streams,
};
use quickfs_fs_adapter::MAX_CALLBACK_XATTR_SIZE;
use quickfs_protocol::{Metadata, Name, NodeKind, XattrSetMode};
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use widestring::U16CStr;
use winfsp_wrs::{
    CreateFileInfo, CreateOptions, FileAccessRights, FileAttributes, FileInfo, WriteMode,
};

/// One open named stream.
pub(super) struct OpenStream {
    pub(super) name: Name,
    value: Mutex<StreamValue>,
}

#[derive(Default)]
struct StreamValue {
    /// The value, once read from the server or created empty.
    bytes: Option<Vec<u8>>,
    /// Whether `bytes` holds changes the server has not seen.
    dirty: bool,
}

impl OpenStream {
    fn new(name: Name, initial: Option<Vec<u8>>) -> Self {
        Self {
            name,
            value: Mutex::new(StreamValue {
                bytes: initial,
                dirty: false,
            }),
        }
    }
}

/// What a path names once named streams are taken into account.
pub(super) struct Target {
    /// The components of the file or directory itself.
    pub(super) components: Vec<String>,
    /// The stream on it, or `None` for the file's own data.
    pub(super) stream: Option<Name>,
}

/// A missing attribute is, to Windows, a missing stream.
fn stream_status(status: NtStatus) -> NtStatus {
    if status == status::STATUS_NO_EAS_ON_FILE {
        status::STATUS_OBJECT_NAME_NOT_FOUND
    } else {
        status
    }
}

impl WindowsFilesystem {
    /// Split a path into the node it names and the stream on it, if any.
    ///
    /// A stored name containing `:` — created on Linux — keeps naming that
    /// file; only when no such entry exists is `a:b` the stream `b` of `a`.
    pub(super) fn stream_target(&self, file_name: &U16CStr) -> Result<Target, NtStatus> {
        let mut components = self.components(file_name)?;
        let Some(last) = components.last() else {
            return Ok(Target {
                components,
                stream: None,
            });
        };
        if !self.capabilities.supports_xattrs || !last.contains(':') {
            return Ok(Target {
                components,
                stream: None,
            });
        }
        if let Ok(existing) = self.block_on(self.resolver.resolve(&components)) {
            self.resolver.release_resolved(&existing);
            return Ok(Target {
                components,
                stream: None,
            });
        }
        let Some(parsed) = streams::split(last).map_err(|_| status::STATUS_OBJECT_NAME_INVALID)?
        else {
            return Ok(Target {
                components,
                stream: None,
            });
        };
        let stream = (!parsed.stream.is_empty()).then(|| Name::from(parsed.stream.to_owned()));
        let base = parsed.base.to_owned();
        if let Some(last) = components.last_mut() {
            *last = base;
        }
        Ok(Target { components, stream })
    }

    /// Open an existing stream on the already-resolved `node`, which this
    /// takes ownership of the reference to.
    pub(super) fn open_stream(
        &self,
        node: Resolved,
        location: Option<Location>,
        components: Vec<String>,
        stream: Name,
        create_options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Handle, FileInfo), NtStatus> {
        let refuse = |status| {
            self.resolver.release_resolved(&node);
            Err(status)
        };
        if create_options.is(CreateOptions::FILE_DIRECTORY_FILE) {
            return refuse(status::STATUS_NOT_A_DIRECTORY);
        }
        if node.kind() == NodeKind::Symlink {
            return refuse(status::STATUS_OBJECT_NAME_NOT_FOUND);
        }
        let size = match self.block_on(self.adapter().xattr_size_async(node.inode, stream.clone()))
        {
            Ok(size) => size,
            Err(error) => return refuse(stream_status(adapter_status(error))),
        };
        let info = self.stream_file_info(&node.metadata, size);
        let context = FileContext::for_stream(
            node.inode,
            granted_access,
            location,
            components,
            OpenStream::new(stream, None),
        );
        if create_options.is(CreateOptions::FILE_DELETE_ON_CLOSE) {
            context.mark_delete_on_close();
        }
        Ok((Handle::new(context), info))
    }

    /// Create a new, empty stream on an existing file or directory.
    pub(super) fn create_stream(
        &self,
        components: &[String],
        stream: Name,
        create_file_info: CreateFileInfo,
    ) -> Result<(Handle, FileInfo), NtStatus> {
        let node = self
            .block_on(self.resolver.resolve(components))
            .map_err(adapter_status)?;
        if node.kind() == NodeKind::Symlink {
            self.resolver.release_resolved(&node);
            return Err(status::STATUS_OBJECT_NAME_INVALID);
        }
        // Created on the server at once, so a racing create sees a collision
        // and another open sees the stream, exactly as on NTFS.
        if let Err(error) = self.block_on(self.adapter().set_xattr_async(
            node.inode,
            stream.clone(),
            Vec::new(),
            XattrSetMode::Create,
            0,
        )) {
            self.resolver.release_resolved(&node);
            return Err(stream_status(adapter_status(error)));
        }
        let info = self.stream_file_info(&node.metadata, 0);
        let context = FileContext::for_stream(
            node.inode,
            create_file_info.granted_access,
            None,
            components.to_vec(),
            OpenStream::new(stream, Some(Vec::new())),
        );
        if create_file_info
            .create_options
            .is(CreateOptions::FILE_DELETE_ON_CLOSE)
        {
            context.mark_delete_on_close();
        }
        Ok((Handle::new(context), info))
    }

    /// File information for a stream: the carrying node's times and
    /// attributes, with the stream's own size and never a directory or link.
    fn stream_file_info(&self, metadata: &Metadata, size: u64) -> FileInfo {
        let mut info = self.file_info(metadata, &Name::default());
        let attributes = info.file_attributes().0
            & !(FileAttributes::DIRECTORY.0 | FileAttributes::REPARSE_POINT.0);
        info.set_file_attributes(FileAttributes(if attributes == 0 {
            FileAttributes::NORMAL.0
        } else {
            attributes
        }))
        .set_reparse_tag(0)
        .set_file_size(size)
        .set_allocation_size(crate::attributes::allocation_size(
            size,
            super::ALLOCATION_UNIT,
        ));
        info
    }

    /// Run `edit` on the stream's value, reading it from the server first if
    /// this open has not needed it yet.
    fn with_value<T>(
        &self,
        context: &FileContext,
        stream: &OpenStream,
        edit: impl FnOnce(&mut StreamValue) -> Result<T, NtStatus>,
    ) -> Result<T, NtStatus> {
        let mut value = stream.value.lock().unwrap_or_else(PoisonError::into_inner);
        if value.bytes.is_none() {
            let loaded = self
                .block_on(
                    self.adapter()
                        .get_xattr_async(context.inode(), stream.name.clone()),
                )
                .map_err(|error| stream_status(adapter_status(error)))?;
            value.bytes = Some(loaded);
        }
        edit(&mut value)
    }

    pub(super) fn stream_read(
        &self,
        context: &FileContext,
        stream: &OpenStream,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<usize, NtStatus> {
        self.with_value(context, stream, |value| {
            let bytes = value.bytes.as_deref().unwrap_or_default();
            let start = usize::try_from(offset).unwrap_or(usize::MAX);
            if start >= bytes.len() {
                return if buffer.is_empty() {
                    Ok(0)
                } else {
                    Err(status::STATUS_END_OF_FILE)
                };
            }
            let length = (bytes.len() - start).min(buffer.len());
            buffer[..length].copy_from_slice(&bytes[start..start + length]);
            Ok(length)
        })
    }

    pub(super) fn stream_write(
        &self,
        context: &FileContext,
        stream: &OpenStream,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, FileInfo), NtStatus> {
        let written = self.with_value(context, stream, |value| {
            let bytes = value.bytes.get_or_insert_with(Vec::new);
            let (offset, data) = match mode {
                WriteMode::Normal { offset } => (offset, buffer),
                WriteMode::WriteToEOF => (bytes.len() as u64, buffer),
                WriteMode::ConstrainedIO { offset } => {
                    let room = (bytes.len() as u64).saturating_sub(offset);
                    let room = usize::try_from(room)
                        .unwrap_or(usize::MAX)
                        .min(buffer.len());
                    (offset, &buffer[..room])
                }
            };
            if data.is_empty() {
                return Ok(0);
            }
            let start = usize::try_from(offset).map_err(|_| status::STATUS_DISK_FULL)?;
            let end = start
                .checked_add(data.len())
                .ok_or(status::STATUS_DISK_FULL)?;
            if end as u64 > MAX_CALLBACK_XATTR_SIZE {
                return Err(status::STATUS_DISK_FULL);
            }
            if bytes.len() < end {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(data);
            value.dirty = true;
            Ok(data.len())
        })?;
        Ok((written, self.stream_info_of(context, stream)?))
    }

    pub(super) fn stream_set_size(
        &self,
        context: &FileContext,
        stream: &OpenStream,
        size: u64,
        allocation_only: bool,
    ) -> Result<FileInfo, NtStatus> {
        self.with_value(context, stream, |value| {
            let bytes = value.bytes.get_or_insert_with(Vec::new);
            let size = usize::try_from(size).map_err(|_| status::STATUS_DISK_FULL)?;
            // As for files, an allocation change only matters when it shrinks.
            if allocation_only && size >= bytes.len() {
                return Ok(());
            }
            if size as u64 > MAX_CALLBACK_XATTR_SIZE {
                return Err(status::STATUS_DISK_FULL);
            }
            if size != bytes.len() {
                bytes.resize(size, 0);
                value.dirty = true;
            }
            Ok(())
        })?;
        self.stream_info_of(context, stream)
    }

    pub(super) fn stream_overwrite(
        &self,
        context: &FileContext,
        stream: &OpenStream,
    ) -> Result<FileInfo, NtStatus> {
        {
            let mut value = stream.value.lock().unwrap_or_else(PoisonError::into_inner);
            value.bytes = Some(Vec::new());
            value.dirty = true;
        }
        self.stream_info_of(context, stream)
    }

    /// Write a modified value back to the server.
    pub(super) fn stream_flush(
        &self,
        context: &FileContext,
        stream: &OpenStream,
    ) -> Result<(), NtStatus> {
        let mut value = stream.value.lock().unwrap_or_else(PoisonError::into_inner);
        if !value.dirty {
            return Ok(());
        }
        let bytes = value.bytes.clone().unwrap_or_default();
        self.block_on(self.adapter().set_xattr_async(
            context.inode(),
            stream.name.clone(),
            bytes,
            XattrSetMode::Upsert,
            0,
        ))
        .map_err(adapter_status)?;
        value.dirty = false;
        Ok(())
    }

    /// The last close of a stream: write it back, or remove it when it was
    /// deleted. Best effort, like every cleanup.
    pub(super) fn stream_cleanup(&self, context: &FileContext, stream: &OpenStream, delete: bool) {
        if delete {
            let _ = self.block_on(
                self.adapter()
                    .remove_xattr_async(context.inode(), stream.name.clone()),
            );
            let mut value = stream.value.lock().unwrap_or_else(PoisonError::into_inner);
            value.dirty = false;
        } else {
            let _ = self.stream_flush(context, stream);
        }
    }

    pub(super) fn stream_info_of(
        &self,
        context: &FileContext,
        stream: &OpenStream,
    ) -> Result<FileInfo, NtStatus> {
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode()))
            .map_err(adapter_status)?;
        let known = stream
            .value
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .bytes
            .as_ref()
            .map(|bytes| bytes.len() as u64);
        let size = match known {
            Some(size) => size,
            None => self
                .block_on(
                    self.adapter()
                        .xattr_size_async(context.inode(), stream.name.clone()),
                )
                .map_err(|error| stream_status(adapter_status(error)))?,
        };
        Ok(self.stream_file_info(&metadata, size))
    }

    /// `GetStreamInfo` for a file or directory: its default data stream, if it
    /// has one, followed by one stream per attribute Windows can spell.
    pub(super) fn list_streams(
        &self,
        context: &FileContext,
        buffer: &mut [u8],
    ) -> Result<usize, NtStatus> {
        let inode = context.inode();
        let metadata = self
            .block_on(self.adapter().getattr_async(inode))
            .map_err(adapter_status)?;
        let mut entries = Vec::new();
        if metadata.kind != NodeKind::Directory {
            entries.push((String::new(), metadata.size));
        }
        if self.capabilities.supports_xattrs && metadata.kind != NodeKind::Symlink {
            let names = self
                .block_on(self.adapter().list_xattrs_async(inode))
                .map_err(adapter_status)?;
            for name in names {
                let Ok(text) = std::str::from_utf8(name.as_bytes()) else {
                    continue;
                };
                if !streams::is_stream_name(text) {
                    continue;
                }
                let Ok(size) = self.block_on(self.adapter().xattr_size_async(inode, name.clone()))
                else {
                    continue;
                };
                entries.push((text.to_owned(), size));
            }
        }
        Ok(streams::pack(&entries, buffer))
    }
}

impl FileContext {
    fn for_stream(
        inode: u64,
        granted_access: FileAccessRights,
        location: Option<Location>,
        path: Vec<String>,
        stream: OpenStream,
    ) -> Arc<Self> {
        // A stream is data, so it is never a directory or a link, whatever
        // carries it.
        Arc::new(Self {
            identity: Mutex::new((inode, NodeKind::File)),
            path: Mutex::new(path),
            granted_access,
            location: Mutex::new(location),
            file: Mutex::new(None),
            directory: Mutex::new(None),
            delete_on_close: AtomicBool::new(false),
            stream: Some(stream),
        })
    }

    fn mark_delete_on_close(&self) {
        self.delete_on_close.store(true, Ordering::Release);
    }
}
