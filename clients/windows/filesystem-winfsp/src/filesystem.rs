// SPDX-License-Identifier: Apache-2.0
//! The WinFsp callback surface.
//!
//! Each callback here does the same three things: turn the Windows request into
//! an adapter operation, run it to completion on the shared runtime, and turn
//! the result back into `NTSTATUS` plus the structures WinFsp expects. All of
//! the interesting filesystem behaviour — caching, coalescing, revision
//! checking, reconnect — already happened one layer down and is shared with the
//! macFUSE client.
//!
//! # Handle lifetime
//!
//! Windows applications keep files open for a long time, open the same file
//! several times at once, and query metadata far more often than they read.
//! Three things follow from that, and they are the reason this file is not
//! simply a set of path-based operations:
//!
//! * A [`FileContext`] is created once per open and carries the resolved inode,
//!   so every later callback on that handle skips path resolution entirely. It
//!   also survives a rename, because an inode names a node and not a path.
//! * The *remote* file handle underneath it is opened lazily, on the first
//!   operation that actually needs one. Explorer opens files purely to read
//!   their attributes; making that cost a `Open` round trip on the server would
//!   turn browsing a folder into one round trip per file.
//! * Cleanup is unconditional. `close` releases the remote handle and the inode
//!   reference whether or not the operations on that handle succeeded, so an
//!   interrupted copy cannot leak a server-side descriptor.

use crate::{
    attributes::{self, allocation_size},
    handle::Handle,
    path::{self, PathError},
    resolve::{Resolved, Resolver},
    status::{self, NtStatus},
};
use quickfs_fs_adapter::{Adapter, AdapterError, DirectoryListing, ROOT_INODE};
use quickfs_protocol::{
    AttributeChanges, FileAccess, FileOpenOptions, FilesystemCapabilities, Metadata, Name,
    NodeKind, RenameMode,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use widestring::{U16CStr, U16CString, U16Str};
use winfsp_wrs::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileInfo, FileSystemInterface, PSecurityDescriptor, SecurityDescriptor, VolumeInfo, WriteMode,
};

/// Bytes per allocation unit reported to Windows. One sector, which is what a
/// modern NTFS volume reports and what Explorer divides by to show "size on
/// disk".
pub const ALLOCATION_UNIT: u64 = 4_096;

/// Default permissions for nodes this client creates. The server applies its
/// own policy on top; these are only the request.
const DEFAULT_FILE_MODE: u32 = 0o644;
const DEFAULT_DIRECTORY_MODE: u32 = 0o755;
const READ_ONLY_FILE_MODE: u32 = 0o444;

/// Where a node currently lives, as far as this handle knows.
///
/// Held separately from the inode because `Cleanup` may arrive with a null file
/// name — WinFsp does not promise one — and a delete still has to name the entry
/// inside its parent. `rename` updates it so a delete-on-close after a rename
/// removes the new name and not the old one.
#[derive(Clone, Debug)]
struct Location {
    parent_inode: u64,
    name: Name,
}

/// An open directory handle and the listing snapshot it pins.
///
/// The snapshot is taken once per open so a paged enumeration — WinFsp calls
/// `ReadDirectory` repeatedly with a marker until the caller stops — sees one
/// consistent directory rather than a listing that may have been refreshed
/// between pages.
struct OpenDirectory {
    handle: u64,
    listing: Arc<DirectoryListing>,
    current: Metadata,
    parent: Metadata,
}

/// Per-open state. One of these exists for each successful `Create` or `Open`
/// and lives until the matching `Close`.
pub struct FileContext {
    inode: u64,
    kind: NodeKind,
    granted_access: FileAccessRights,
    location: Mutex<Option<Location>>,
    /// The adapter's logical file handle, opened on first use.
    file: Mutex<Option<u64>>,
    /// The adapter's directory handle and the snapshot it pins, opened on the
    /// first enumeration.
    directory: Mutex<Option<OpenDirectory>>,
    delete_on_close: AtomicBool,
}

impl FileContext {
    fn new(
        resolved: &Resolved,
        granted_access: FileAccessRights,
        location: Option<Location>,
    ) -> Arc<Self> {
        Arc::new(Self {
            inode: resolved.inode,
            kind: resolved.kind(),
            granted_access,
            location: Mutex::new(location),
            file: Mutex::new(None),
            directory: Mutex::new(None),
            delete_on_close: AtomicBool::new(false),
        })
    }

    fn is_directory(&self) -> bool {
        self.kind == NodeKind::Directory
    }

    /// Whether this open was granted enough access to modify file contents.
    fn writable(&self) -> bool {
        self.granted_access.is(FileAccessRights::FILE_WRITE_DATA)
            || self.granted_access.is(FileAccessRights::FILE_APPEND_DATA)
    }
}

/// Security descriptors derived from POSIX modes, built once per distinct
/// access mask.
///
/// There are at most a handful of distinct masks — a mode maps onto read,
/// write, execute and delete and nothing finer — so this cache is small and
/// bounded no matter how many files the mount has. The descriptors are leaked
/// deliberately: WinFsp's `GetSecurityByName` returns a bare pointer that it
/// copies out of during the call, and a descriptor that lives for the process
/// is the simplest thing that is definitely still valid at that moment.
struct SecurityDescriptors {
    sid: String,
    by_mask: Mutex<HashMap<u32, &'static SecurityDescriptor>>,
}

impl SecurityDescriptors {
    fn new(sid: String) -> Self {
        Self {
            sid,
            by_mask: Mutex::new(HashMap::new()),
        }
    }

    fn for_node(&self, mode: u32, kind: NodeKind) -> Result<PSecurityDescriptor, NtStatus> {
        let mask = attributes::access_mask(mode, kind);
        let mut cache = self
            .by_mask
            .lock()
            .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        if let Some(descriptor) = cache.get(&mask) {
            return Ok(descriptor.as_ptr());
        }
        let sid = &self.sid;
        // `D:P` protects the DACL so nothing is inherited from the volume, and
        // the single ACE names only the mounting account: no `Everyone`, no
        // approximation of the POSIX group and other bits onto trustees the
        // user did not choose.
        let sddl = if mask == 0 {
            format!("O:{sid}G:{sid}D:P")
        } else {
            format!("O:{sid}G:{sid}D:P(A;OICI;0x{mask:x};;;{sid})")
        };
        let text = U16CString::from_str(&sddl).map_err(|_| status::STATUS_INVALID_PARAMETER)?;
        let descriptor =
            SecurityDescriptor::from_wstr(&text).map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        let descriptor: &'static SecurityDescriptor = Box::leak(Box::new(descriptor));
        cache.insert(mask, descriptor);
        Ok(descriptor.as_ptr())
    }
}

/// One mounted quicKFS export, presented to Windows.
pub struct WindowsFilesystem {
    resolver: Resolver,
    security: SecurityDescriptors,
    capabilities: FilesystemCapabilities,
    volume_label: U16CString,
}

impl WindowsFilesystem {
    pub fn new(
        adapter: Adapter,
        capabilities: FilesystemCapabilities,
        volume_label: &str,
        owner_sid: String,
    ) -> Result<Self, PathError> {
        let volume_label =
            U16CString::from_str(volume_label).map_err(|_| PathError::NotRepresentable)?;
        Ok(Self {
            resolver: Resolver::new(adapter),
            security: SecurityDescriptors::new(owner_sid),
            capabilities,
            volume_label,
        })
    }

    pub fn adapter(&self) -> &Adapter {
        self.resolver.adapter()
    }

    pub fn resolver(&self) -> &Resolver {
        &self.resolver
    }

    /// Run one adapter operation to completion.
    ///
    /// WinFsp dispatches each callback on its own thread pool, never on a Tokio
    /// worker, so blocking here cannot deadlock the runtime the adapter shares
    /// with the transport. The operation's own deadline is enforced inside the
    /// adapter, so this cannot block for longer than the configured callback
    /// timeout.
    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.adapter().runtime().block_on(future)
    }

    fn components(&self, file_name: &U16CStr) -> Result<Vec<String>, NtStatus> {
        path::components(file_name.as_slice()).map_err(path_status)
    }

    /// Resolve a path, returning the node without holding a reference the
    /// caller has to remember to release. Directories keep their reference in
    /// the resolver's retained set, which is what keeps their listing cached.
    fn resolve_transient(&self, file_name: &U16CStr) -> Result<Resolved, NtStatus> {
        let components = self.components(file_name)?;
        let resolved = self
            .block_on(self.resolver.resolve(&components))
            .map_err(adapter_status)?;
        self.resolver.release_resolved(&resolved);
        Ok(resolved)
    }

    fn file_info(&self, metadata: &Metadata, name: &Name) -> FileInfo {
        let mut info = FileInfo::default();
        info.set_file_attributes(FileAttributes(attributes::file_attributes(
            metadata.kind,
            metadata.mode,
            path::is_hidden_name(name.as_bytes()),
        )))
        .set_file_size(metadata.size)
        .set_allocation_size(allocation_size(metadata.size, ALLOCATION_UNIT))
        .set_creation_time(attributes::filetime_from_unix_ms(
            metadata
                .created_unix_ms
                .unwrap_or(metadata.modified_unix_ms),
        ))
        .set_last_access_time(attributes::filetime_from_unix_ms(metadata.accessed_unix_ms))
        .set_last_write_time(attributes::filetime_from_unix_ms(metadata.modified_unix_ms))
        .set_change_time(attributes::filetime_from_unix_ms(metadata.modified_unix_ms))
        // The protocol node id is a UUID and does not fit a 64-bit index, so
        // the adapter's stable inode is reported instead. It is unique and
        // stable for the life of the mount, which is what an application uses
        // it for.
        .set_index_number(0)
        .set_hard_links(0)
        .set_ea_size(0);
        info
    }

    fn info_for(&self, inode: u64, name: &Name) -> Result<FileInfo, NtStatus> {
        let metadata = self
            .block_on(self.adapter().getattr_async(inode))
            .map_err(adapter_status)?;
        Ok(self.file_info(&metadata, name))
    }

    /// The adapter file handle for this context, opening one if this is the
    /// first operation that needs it.
    fn file_handle(&self, context: &FileContext) -> Result<u64, NtStatus> {
        let mut slot = context
            .file
            .lock()
            .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        if let Some(handle) = *slot {
            return Ok(handle);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let access = if context.writable() {
            FileAccess::ReadWrite
        } else {
            FileAccess::ReadOnly
        };
        let options = FileOpenOptions {
            access,
            truncate: false,
            append: false,
        };
        let handle = self
            .block_on(self.adapter().open_async(context.inode, options))
            .map_err(adapter_status)?;
        *slot = Some(handle);
        Ok(handle)
    }

    /// The directory snapshot for this context, opening one if this is the
    /// first enumeration.
    fn directory_snapshot(
        &self,
        context: &FileContext,
    ) -> Result<(Arc<DirectoryListing>, Metadata, Metadata), NtStatus> {
        let mut slot = context
            .directory
            .lock()
            .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        if let Some(open) = slot.as_ref() {
            return Ok((
                Arc::clone(&open.listing),
                open.current.clone(),
                open.parent.clone(),
            ));
        }
        if !context.is_directory() {
            return Err(status::STATUS_NOT_A_DIRECTORY);
        }
        let handle = self
            .block_on(self.adapter().opendir_async(context.inode))
            .map_err(adapter_status)?;
        let (listing, current, parent) = self
            .adapter()
            .directory_listing_with_metadata(handle, context.inode)
            .map_err(|error| {
                let _ = self.adapter().releasedir(handle);
                adapter_status(error)
            })?;
        *slot = Some(OpenDirectory {
            handle,
            listing: Arc::clone(&listing),
            current: current.clone(),
            parent: parent.clone(),
        });
        Ok((listing, current, parent))
    }

    fn location_of(&self, context: &FileContext) -> Option<Location> {
        context
            .location
            .lock()
            .ok()
            .and_then(|location| location.clone())
    }

    fn set_location(&self, context: &FileContext, location: Location) {
        if let Ok(mut slot) = context.location.lock() {
            *slot = Some(location);
        }
    }

    /// Resolve a path's parent directory and final name for a mutation.
    fn parent_and_name(
        &self,
        file_name: &U16CStr,
        creating: bool,
    ) -> Result<(Resolved, Name), NtStatus> {
        let (parent_components, name) =
            path::split_parent(file_name.as_slice()).map_err(path_status)?;
        if creating {
            path::validate_creatable(&name).map_err(path_status)?;
        }
        let parent = self
            .block_on(self.resolver.resolve_parent(&parent_components))
            .map_err(adapter_status)?;
        Ok((parent, Name::from(name)))
    }

    fn set_modified_now(&self, inode: u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        let changes = AttributeChanges {
            size: None,
            mode: None,
            accessed_unix_ms: None,
            modified_unix_ms: Some(now),
            backup_unix_ms: None,
        };
        let _ = self.block_on(self.adapter().setattr_async(inode, None, changes));
    }
}

fn path_status(error: PathError) -> NtStatus {
    match error {
        PathError::NotAbsolute | PathError::EmptyComponent | PathError::RelativeComponent => {
            status::STATUS_OBJECT_PATH_NOT_FOUND
        }
        PathError::IsRoot => status::STATUS_ACCESS_DENIED,
        PathError::ComponentTooLong
        | PathError::NotRepresentable
        | PathError::InvalidCharacter
        | PathError::TrailingDotOrSpace
        | PathError::ReservedName => status::STATUS_OBJECT_NAME_INVALID,
    }
}

fn adapter_status(error: AdapterError) -> NtStatus {
    status::status_from_adapter_error(&error)
}

impl FileSystemInterface for WindowsFilesystem {
    type FileContext = Handle;

    const GET_VOLUME_INFO_DEFINED: bool = true;
    const GET_SECURITY_BY_NAME_DEFINED: bool = true;
    const CREATE_DEFINED: bool = true;
    const OPEN_DEFINED: bool = true;
    const OVERWRITE_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;
    const CLOSE_DEFINED: bool = true;
    const READ_DEFINED: bool = true;
    const WRITE_DEFINED: bool = true;
    const FLUSH_DEFINED: bool = true;
    const GET_FILE_INFO_DEFINED: bool = true;
    const SET_BASIC_INFO_DEFINED: bool = true;
    const SET_FILE_SIZE_DEFINED: bool = true;
    const RENAME_DEFINED: bool = true;
    const GET_SECURITY_DEFINED: bool = true;
    const READ_DIRECTORY_DEFINED: bool = true;
    const GET_DIR_INFO_BY_NAME_DEFINED: bool = true;
    const SET_DELETE_DEFINED: bool = true;

    fn get_volume_info(&self) -> Result<VolumeInfo, NtStatus> {
        let statistics = self
            .block_on(self.adapter().statfs_async())
            .map_err(adapter_status)?;
        let block = u64::from(statistics.block_size.max(1));
        let total = statistics.blocks.saturating_mul(block);
        let free = statistics.blocks_available.saturating_mul(block);
        VolumeInfo::new(
            total,
            free,
            U16Str::from_slice(self.volume_label.as_slice()),
        )
        .map_err(|_| status::STATUS_INVALID_PARAMETER)
    }

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _find_reparse_point: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool), NtStatus> {
        let resolved = self.resolve_transient(file_name)?;
        let name = final_name(file_name);
        let attributes = FileAttributes(attributes::file_attributes(
            resolved.metadata.kind,
            resolved.metadata.mode,
            path::is_hidden_name(name.as_bytes()),
        ));
        let descriptor = self
            .security
            .for_node(resolved.metadata.mode, resolved.metadata.kind)?;
        Ok((attributes, descriptor, false))
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_file_info: CreateFileInfo,
        _security_descriptor: SecurityDescriptor,
    ) -> Result<(Self::FileContext, FileInfo), NtStatus> {
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        let (parent, name) = self.parent_and_name(file_name, true)?;
        let parent_inode = parent.inode;
        let result = if create_file_info
            .create_options
            .is(CreateOptions::FILE_DIRECTORY_FILE)
        {
            self.create_directory(parent_inode, &name, create_file_info)
        } else {
            self.create_file(parent_inode, &name, create_file_info)
        };
        self.resolver.release_resolved(&parent);
        let (context, info) = result?;
        self.set_location(&context, Location { parent_inode, name });
        Ok((Handle::new(context), info))
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), NtStatus> {
        let components = self.components(file_name)?;
        // Resolving the parent and then looking the child up inside it costs
        // exactly the same walk as resolving the whole path, and it yields the
        // parent inode this handle will need if it is later deleted or
        // renamed. Doing it in two steps here saves doing it again then.
        let (resolved, location) = match components.split_last() {
            None => (
                self.block_on(self.resolver.resolve(&[]))
                    .map_err(adapter_status)?,
                None,
            ),
            Some((name, parent_components)) => {
                let parent = self
                    .block_on(self.resolver.resolve_parent(parent_components))
                    .map_err(adapter_status)?;
                let name = Name::from(name.clone());
                let looked_up =
                    self.block_on(self.adapter().lookup_async(parent.inode, name.clone()));
                let location = Location {
                    parent_inode: parent.inode,
                    name,
                };
                // The parent's reference goes back whether or not the child was
                // found, so a probe for an absent file leaks nothing.
                self.resolver.release_resolved(&parent);
                let looked_up = looked_up.map_err(adapter_status)?;
                (
                    Resolved {
                        inode: looked_up.inode,
                        metadata: looked_up.metadata,
                    },
                    Some(location),
                )
            }
        };

        // Windows asks for a directory or a non-directory explicitly, and an
        // application that opened a directory expecting a file misbehaves in
        // ways that are hard to trace back here.
        let mismatch =
            if create_options.is(CreateOptions::FILE_DIRECTORY_FILE) && !resolved.is_directory() {
                Some(status::STATUS_NOT_A_DIRECTORY)
            } else if create_options.is(CreateOptions::FILE_NON_DIRECTORY_FILE)
                && resolved.is_directory()
            {
                Some(status::STATUS_FILE_IS_A_DIRECTORY)
            } else {
                None
            };
        if let Some(status) = mismatch {
            self.resolver.release_resolved(&resolved);
            return Err(status);
        }

        let name = location
            .as_ref()
            .map(|location| location.name.clone())
            .unwrap_or_default();
        let info = self.file_info(&resolved.metadata, &name);
        let context = FileContext::new(&resolved, granted_access, location);
        if create_options.is(CreateOptions::FILE_DELETE_ON_CLOSE) {
            context.delete_on_close.store(true, Ordering::Release);
        }
        Ok((Handle::new(context), info))
    }

    fn overwrite(
        &self,
        context: Self::FileContext,
        _file_attributes: FileAttributes,
        _replace_file_attributes: bool,
        _allocation_size: u64,
    ) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let handle = self.file_handle(&context)?;
        let changes = AttributeChanges {
            size: Some(0),
            mode: None,
            accessed_unix_ms: None,
            modified_unix_ms: None,
            backup_unix_ms: None,
        };
        let metadata = self
            .block_on(
                self.adapter()
                    .setattr_async(context.inode, Some(handle), changes),
            )
            .map_err(adapter_status)?;
        Ok(self.file_info(&metadata, &self.name_of(&context)))
    }

    fn cleanup(
        &self,
        context: Self::FileContext,
        _file_name: Option<&U16CStr>,
        flags: CleanupFlags,
    ) {
        let Some(context) = context.get().cloned() else {
            return;
        };
        // Cleanup runs when the last user handle closes. It is the only point
        // at which a Windows delete actually happens, and it must not fail the
        // way an ordinary callback can — there is nobody left to report to — so
        // every step here is best effort and errors are swallowed after being
        // attempted.
        if flags.is(CleanupFlags::SET_LAST_WRITE_TIME) || flags.is(CleanupFlags::SET_CHANGE_TIME) {
            self.set_modified_now(context.inode);
        }
        if (flags.is(CleanupFlags::DELETE) || context.delete_on_close.load(Ordering::Acquire))
            && let Some(location) = self.location_of(&context)
        {
            let _ = self.block_on(self.adapter().remove_async(
                location.parent_inode,
                location.name,
                context.is_directory(),
            ));
        }
    }

    fn close(&self, context: Self::FileContext) {
        let Some(context) = context.get().cloned() else {
            return;
        };
        // Unconditional teardown: an interrupted or failed operation must not
        // leave a server-side descriptor or an inode reference behind.
        if let Ok(mut slot) = context.file.lock()
            && let Some(handle) = slot.take()
        {
            let _ = self.block_on(
                self.adapter()
                    .release_async(handle, context.writable(), None),
            );
        }
        if let Ok(mut slot) = context.directory.lock()
            && let Some(open) = slot.take()
        {
            let _ = self.adapter().releasedir(open.handle);
        }
        self.resolver.release(context.inode);
    }

    fn read(
        &self,
        context: Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<usize, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let handle = self.file_handle(&context)?;
        let length = u64::try_from(buffer.len()).map_err(|_| status::STATUS_INVALID_PARAMETER)?;
        let data = self
            .block_on(self.adapter().read_async(handle, offset, length))
            .map_err(adapter_status)?;
        if data.is_empty() && !buffer.is_empty() {
            // Windows distinguishes "read nothing because the file ended" from
            // "read nothing successfully"; applications loop forever on the
            // latter.
            return Err(status::STATUS_END_OF_FILE);
        }
        let length = data.len().min(buffer.len());
        buffer[..length].copy_from_slice(&data[..length]);
        Ok(length)
    }

    fn write(
        &self,
        context: Self::FileContext,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, FileInfo), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let handle = self.file_handle(&context)?;
        let current = self
            .block_on(self.adapter().getattr_async(context.inode))
            .map_err(adapter_status)?;

        let (offset, buffer) = match mode {
            WriteMode::Normal { offset } => (offset, buffer),
            WriteMode::WriteToEOF => (current.size, buffer),
            WriteMode::ConstrainedIO { offset } => {
                // The caller has promised not to extend the file, so a write
                // past the end is truncated rather than growing it. Returning
                // the clamped count is what tells the FSD how much landed.
                if offset >= current.size {
                    return Ok((0, self.file_info(&current, &self.name_of(&context))));
                }
                let room = (current.size - offset).min(buffer.len() as u64) as usize;
                (offset, &buffer[..room])
            }
        };
        if buffer.is_empty() {
            return Ok((0, self.file_info(&current, &self.name_of(&context))));
        }
        let written = self
            .block_on(self.adapter().write_async(handle, offset, buffer))
            .map_err(adapter_status)?;
        let written = usize::try_from(written).map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode))
            .map_err(adapter_status)?;
        Ok((written, self.file_info(&metadata, &self.name_of(&context))))
    }

    fn flush(&self, context: Self::FileContext) -> Result<FileInfo, NtStatus> {
        // A null context is WinFsp asking to flush the whole volume. quicKFS
        // has no volume-wide buffer of its own — each handle's coalesced bytes
        // are flushed with that handle — so there is nothing to do beyond
        // succeeding.
        let Some(context) = context.get().cloned() else {
            return Ok(FileInfo::default());
        };
        if context.is_directory() {
            return self.info_for(context.inode, &self.name_of(&context));
        }
        let handle = {
            let slot = context
                .file
                .lock()
                .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
            *slot
        };
        if let Some(handle) = handle {
            self.block_on(self.adapter().fsync_async(handle, false))
                .map_err(adapter_status)?;
        }
        self.info_for(context.inode, &self.name_of(&context))
    }

    fn get_file_info(&self, context: Self::FileContext) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        self.info_for(context.inode, &self.name_of(&context))
    }

    fn set_basic_info(
        &self,
        context: Self::FileContext,
        file_attributes: FileAttributes,
        _creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        _change_time: u64,
    ) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        let current = self
            .block_on(self.adapter().getattr_async(context.inode))
            .map_err(adapter_status)?;

        // `INVALID_FILE_ATTRIBUTES` means "leave the attributes alone"; a zero
        // timestamp means the same for that timestamp. Everything Windows can
        // ask for that quicKFS cannot store — hidden, system, archive — is
        // accepted and dropped rather than refused, because failing here breaks
        // ordinary file copies for a bit nothing can observe afterwards.
        let mode = if file_attributes == FileAttributes::INVALID {
            None
        } else {
            attributes::apply_readonly(current.mode, file_attributes.is(FileAttributes::READONLY))
        };
        let changes = AttributeChanges {
            size: None,
            mode,
            accessed_unix_ms: attributes::unix_ms_from_filetime(last_access_time),
            modified_unix_ms: attributes::unix_ms_from_filetime(last_write_time),
            backup_unix_ms: None,
        };
        if changes.mode.is_none()
            && changes.accessed_unix_ms.is_none()
            && changes.modified_unix_ms.is_none()
        {
            // Nothing quicKFS can store changed. Windows issues this callback
            // on nearly every close, so answering from what we already know
            // keeps it off the network entirely.
            return Ok(self.file_info(&current, &self.name_of(&context)));
        }
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        let metadata = self
            .block_on(self.adapter().setattr_async(context.inode, None, changes))
            .map_err(adapter_status)?;
        Ok(self.file_info(&metadata, &self.name_of(&context)))
    }

    fn set_file_size(
        &self,
        context: Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
    ) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let current = self
            .block_on(self.adapter().getattr_async(context.inode))
            .map_err(adapter_status)?;

        // An allocation-size change is a hint about reserved space, not about
        // the file's length, and quicKFS does not expose reserved space. The
        // one case where it is not a hint is a shrink below the current length,
        // which NTFS treats as a truncation and applications rely on.
        let target = if set_allocation_size {
            if new_size >= current.size {
                return Ok(self.file_info(&current, &self.name_of(&context)));
            }
            new_size
        } else {
            new_size
        };
        if target == current.size {
            return Ok(self.file_info(&current, &self.name_of(&context)));
        }
        let handle = {
            let slot = context
                .file
                .lock()
                .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
            *slot
        };
        let changes = AttributeChanges {
            size: Some(target),
            mode: None,
            accessed_unix_ms: None,
            modified_unix_ms: None,
            backup_unix_ms: None,
        };
        let metadata = self
            .block_on(self.adapter().setattr_async(context.inode, handle, changes))
            .map_err(adapter_status)?;
        Ok(self.file_info(&metadata, &self.name_of(&context)))
    }

    fn set_delete(
        &self,
        context: Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        if delete_file && context.is_directory() {
            // Windows requires a directory to be empty at the moment the flag
            // is set, not at cleanup, so an application learns immediately that
            // its delete will not succeed.
            let (listing, _, _) = self.directory_snapshot(&context)?;
            if !listing.entries.is_empty() {
                return Err(status::STATUS_DIRECTORY_NOT_EMPTY);
            }
        }
        context
            .delete_on_close
            .store(delete_file, Ordering::Release);
        Ok(())
    }

    fn rename(
        &self,
        context: Self::FileContext,
        _file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        let source = self
            .location_of(&context)
            .ok_or(status::STATUS_INVALID_HANDLE)?;
        let (destination, new_name) = self.parent_and_name(new_file_name, true)?;
        let destination_inode = destination.inode;
        let mode = if replace_if_exists {
            RenameMode::Replace
        } else {
            RenameMode::NoReplace
        };
        let result = self.block_on(self.adapter().rename_async(
            source.parent_inode,
            source.name,
            destination_inode,
            new_name.clone(),
            mode,
        ));
        self.resolver.release_resolved(&destination);
        result.map_err(adapter_status)?;
        // The handle stays valid across the rename because it names an inode,
        // not a path; only where a later delete should look has changed.
        self.set_location(
            &context,
            Location {
                parent_inode: destination_inode,
                name: new_name,
            },
        );
        Ok(())
    }

    fn get_security(&self, context: Self::FileContext) -> Result<PSecurityDescriptor, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode))
            .map_err(adapter_status)?;
        self.security.for_node(metadata.mode, metadata.kind)
    }

    fn read_directory(
        &self,
        context: Self::FileContext,
        marker: Option<&U16CStr>,
        mut add_dir_info: impl FnMut(DirInfo) -> bool,
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        let (listing, current, parent) = self.directory_snapshot(&context)?;
        let marker = marker.map(|marker| marker.as_slice().to_vec());
        // Everything before and including the marker was returned by an earlier
        // call on this same snapshot, so enumeration resumes rather than
        // repeating.
        let mut resuming = marker.is_some();

        let mut emit = |name: Vec<u16>, info: FileInfo| -> Option<bool> {
            if resuming {
                if Some(&name) == marker.as_ref() {
                    resuming = false;
                }
                return Some(true);
            }
            let name = U16CString::from_vec(name).ok()?;
            Some(add_dir_info(DirInfo::new(info, &name)))
        };

        // `.` and `..` are part of a Windows directory listing for everything
        // except the volume root, whose parent lies outside the export.
        if context.inode != ROOT_INODE {
            let dot = self.file_info(&current, &Name::from(".".to_owned()));
            if emit(vec![u16::from(b'.')], dot) == Some(false) {
                return Ok(());
            }
            let dotdot = self.file_info(&parent, &Name::from("..".to_owned()));
            if emit(vec![u16::from(b'.'), u16::from(b'.')], dotdot) == Some(false) {
                return Ok(());
            }
        }

        for entry in &listing.entries {
            // A name the Windows namespace cannot express is skipped rather
            // than mangled: a mangled name would appear in Explorer and then
            // fail to open. This is a documented limitation of the Windows
            // client, not a silent one.
            let Some(name) = path::encode_entry_name(entry.name.as_bytes()) else {
                continue;
            };
            let info = self.file_info(&entry.metadata, &entry.name);
            match emit(name, info) {
                Some(true) => {}
                // The buffer is full; WinFsp will call again with the last
                // name as its marker.
                Some(false) => return Ok(()),
                None => continue,
            }
        }
        Ok(())
    }

    fn get_dir_info_by_name(
        &self,
        context: Self::FileContext,
        file_name: &U16CStr,
    ) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        // Explorer asks for one entry at a time far more often than it
        // enumerates. Answering from the parent's cached listing is what keeps
        // that from becoming one round trip per query.
        let name = file_name
            .to_string()
            .map_err(|_| status::STATUS_OBJECT_NAME_INVALID)?;
        path::components_from_str(&format!("\\{name}")).map_err(path_status)?;
        let result = self
            .block_on(
                self.adapter()
                    .lookup_async(context.inode, Name::from(name.clone())),
            )
            .map_err(adapter_status)?;
        let info = self.file_info(&result.metadata, &Name::from(name));
        self.resolver.release(result.inode);
        Ok(info)
    }

    const DISPATCHER_STOPPED_DEFINED: bool = true;

    fn dispatcher_stopped(&self, _normally: bool) {
        // The volume is going away: return every retained translation and close
        // whatever server handles are still open, so an abrupt unmount does not
        // leave descriptors behind on the server.
        self.resolver.release_all();
        self.adapter().destroy_mount();
    }
}

impl WindowsFilesystem {
    fn name_of(&self, context: &FileContext) -> Name {
        self.location_of(context)
            .map(|location| location.name)
            .unwrap_or_default()
    }

    fn create_file(
        &self,
        parent_inode: u64,
        name: &Name,
        create_file_info: CreateFileInfo,
    ) -> Result<(Arc<FileContext>, FileInfo), NtStatus> {
        let mode = if create_file_info
            .file_attributes
            .is(FileAttributes::READONLY)
        {
            READ_ONLY_FILE_MODE
        } else {
            DEFAULT_FILE_MODE
        };
        let access = if create_file_info
            .granted_access
            .is(FileAccessRights::FILE_WRITE_DATA)
            || create_file_info
                .granted_access
                .is(FileAccessRights::FILE_APPEND_DATA)
        {
            FileAccess::ReadWrite
        } else {
            FileAccess::ReadOnly
        };
        let created = self
            .block_on(self.adapter().create_file_async(
                parent_inode,
                name.clone(),
                mode,
                FileOpenOptions {
                    access,
                    truncate: false,
                    append: false,
                },
            ))
            .map_err(adapter_status)?;
        let info = self.file_info(&created.metadata, name);
        let resolved = Resolved {
            inode: created.inode,
            metadata: created.metadata,
        };
        let context = FileContext::new(&resolved, create_file_info.granted_access, None);
        if let Ok(mut slot) = context.file.lock() {
            *slot = Some(created.handle);
        }
        if create_file_info
            .create_options
            .is(CreateOptions::FILE_DELETE_ON_CLOSE)
        {
            context.delete_on_close.store(true, Ordering::Release);
        }
        Ok((context, info))
    }

    fn create_directory(
        &self,
        parent_inode: u64,
        name: &Name,
        create_file_info: CreateFileInfo,
    ) -> Result<(Arc<FileContext>, FileInfo), NtStatus> {
        let created = self
            .block_on(self.adapter().create_directory_async(
                parent_inode,
                name.clone(),
                DEFAULT_DIRECTORY_MODE,
            ))
            .map_err(adapter_status)?;
        let info = self.file_info(&created.metadata, name);
        let resolved = Resolved {
            inode: created.inode,
            metadata: created.metadata,
        };
        let context = FileContext::new(&resolved, create_file_info.granted_access, None);
        if create_file_info
            .create_options
            .is(CreateOptions::FILE_DELETE_ON_CLOSE)
        {
            context.delete_on_close.store(true, Ordering::Release);
        }
        Ok((context, info))
    }
}

/// WinFsp dispatches callbacks from its own thread pool and shares one file
/// system object and one file context across them, so both have to be `Sync`.
/// The wrapper crate's trait does not require it, which means a field that is
/// not would compile and then race at run time; these assertions turn that into
/// a build failure instead.
const _: fn() = || {
    fn assert_shareable<T: Send + Sync>() {}
    assert_shareable::<WindowsFilesystem>();
    assert_shareable::<FileContext>();
};

/// The last component of a WinFsp path, for the hidden-attribute convention.
/// A path that will not parse yields an empty name, which is never hidden.
fn final_name(file_name: &U16CStr) -> Name {
    path::split_parent(file_name.as_slice())
        .map(|(_, name)| Name::from(name))
        .unwrap_or_default()
}
