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
    reparse,
    resolve::{Resolved, Resolver},
    status::{self, NtStatus},
};
use quickfs_fs_adapter::{Adapter, AdapterError, DirectoryListing, ROOT_INODE};
use quickfs_protocol::{
    AttributeChanges, FileAccess, FileOpenOptions, FilesystemCapabilities, Metadata, Name, NodeId,
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

mod stream_ops;
use stream_ops::OpenStream;

/// `DACL_SECURITY_INFORMATION`.
const DACL_SECURITY_INFORMATION: u32 = 0x4;

/// Bytes per allocation unit reported to Windows. One sector, which is what a
/// modern NTFS volume reports and what Explorer divides by to show "size on
/// disk".
pub const ALLOCATION_UNIT: u64 = 4_096;

/// Largest piece of one read or write handed to the adapter at a time. Windows
/// forwards an application's request at its full size, and the adapter bounds
/// a single callback's I/O.
const MAX_IO_PIECE: usize = quickfs_fs_adapter::MAX_CALLBACK_IO_SIZE as usize;

/// Links followed when deciding whether a link points at a directory.
const MAX_LINK_HOPS: usize = 8;

/// Link-kind answers remembered before the cache starts over.
const MAX_CACHED_LINK_KINDS: usize = 4_096;

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
    /// The node this open names and its kind. Fixed for the life of the open,
    /// except that `SetReparsePoint` replaces a freshly created placeholder
    /// with the link it was created to become.
    identity: Mutex<(u64, NodeKind)>,
    /// Components of the path this open was made through, kept current across
    /// renames. A link's target is resolved relative to its directory.
    path: Mutex<Vec<String>>,
    granted_access: FileAccessRights,
    location: Mutex<Option<Location>>,
    /// The adapter's logical file handle, opened on first use.
    file: Mutex<Option<u64>>,
    /// The adapter's directory handle and the snapshot it pins, opened on the
    /// first enumeration.
    directory: Mutex<Option<OpenDirectory>>,
    delete_on_close: AtomicBool,
    /// Whether data was written or the size changed through this open.
    modified: AtomicBool,
    /// Whether the application set a last-write time through this open, which
    /// must then survive the open's close rather than be stamped over.
    explicit_write_time: AtomicBool,
    /// Set when this open names a stream rather than the node's own data.
    stream: Option<OpenStream>,
    /// The stored spelling of the path this open names, reported to WinFsp
    /// for file name normalization.
    normalized: Mutex<Option<U16CString>>,
}

impl FileContext {
    fn new(
        resolved: &Resolved,
        granted_access: FileAccessRights,
        location: Option<Location>,
        path: Vec<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            identity: Mutex::new((resolved.inode, resolved.kind())),
            path: Mutex::new(path),
            granted_access,
            location: Mutex::new(location),
            file: Mutex::new(None),
            directory: Mutex::new(None),
            delete_on_close: AtomicBool::new(false),
            modified: AtomicBool::new(false),
            explicit_write_time: AtomicBool::new(false),
            stream: None,
            normalized: Mutex::new(None),
        })
    }

    fn inode(&self) -> u64 {
        self.identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
    }

    fn kind(&self) -> NodeKind {
        self.identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1
    }

    fn replace_identity(&self, inode: u64, kind: NodeKind) {
        *self
            .identity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (inode, kind);
    }

    fn path(&self) -> Vec<String> {
        self.path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn set_path(&self, path: Vec<String>) {
        *self
            .path
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
    }

    fn is_directory(&self) -> bool {
        self.kind() == NodeKind::Directory
    }

    fn set_normalized(&self, name: Option<U16CString>) {
        *self
            .normalized
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = name;
    }

    fn is_symlink(&self) -> bool {
        self.kind() == NodeKind::Symlink
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
    by_mask: Mutex<HashMap<(u32, bool), &'static SecurityDescriptor>>,
}

impl SecurityDescriptors {
    /// A fresh, owned copy of the descriptor for this mode and kind.
    fn owned(&self, mode: u32, kind: NodeKind) -> Result<SecurityDescriptor, NtStatus> {
        let mask = attributes::access_mask(mode, kind);
        let sid = &self.sid;
        // `D:P` protects the DACL so nothing is inherited from the volume, and
        // the single ACE names only the mounting account: no `Everyone`, no
        // approximation of the POSIX group and other bits onto trustees the
        // user did not choose.
        //
        // Inheritance flags only mean something on a directory. On a file they
        // make the ACE look different from the plain one `icacls /grant:r`
        // writes, so the tool would add its ACE beside this one instead of
        // replacing it — and the old rights would survive the change.
        let inherit = if kind == NodeKind::Directory {
            "OICI"
        } else {
            ""
        };
        let sddl = if mask == 0 {
            format!("O:{sid}G:{sid}D:P")
        } else {
            format!("O:{sid}G:{sid}D:P(A;{inherit};0x{mask:x};;;{sid})")
        };
        let text = U16CString::from_str(&sddl).map_err(|_| status::STATUS_INVALID_PARAMETER)?;
        SecurityDescriptor::from_wstr(&text).map_err(|_| status::STATUS_IO_DEVICE_ERROR)
    }

    fn new(sid: String) -> Self {
        Self {
            sid,
            by_mask: Mutex::new(HashMap::new()),
        }
    }

    fn for_node(&self, mode: u32, kind: NodeKind) -> Result<PSecurityDescriptor, NtStatus> {
        let key = (
            attributes::access_mask(mode, kind),
            kind == NodeKind::Directory,
        );
        let mut cache = self
            .by_mask
            .lock()
            .map_err(|_| status::STATUS_IO_DEVICE_ERROR)?;
        if let Some(descriptor) = cache.get(&key) {
            return Ok(descriptor.as_ptr());
        }
        let descriptor = self.owned(mode, kind)?;
        let descriptor: &'static SecurityDescriptor = Box::leak(Box::new(descriptor));
        cache.insert(key, descriptor);
        Ok(descriptor.as_ptr())
    }
}

/// One mounted quicKFS export, presented to Windows.
pub struct WindowsFilesystem {
    resolver: Resolver,
    security: SecurityDescriptors,
    /// Whether each link (by node and revision) points at a directory, which
    /// Windows needs to know to present it as a directory link.
    link_kinds: Mutex<HashMap<(NodeId, u64), bool>>,
    capabilities: FilesystemCapabilities,
    volume_label: Mutex<U16CString>,
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
            link_kinds: Mutex::new(HashMap::new()),
            capabilities,
            volume_label: Mutex::new(volume_label),
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
        // The protocol node id is a persistent keyed digest of the server's
        // device and inode, so folding it to 64 bits gives a file ID that is
        // stable across remounts and reconnects, as NTFS file IDs are. Tools
        // that detect renames or hard links by file ID depend on that.
        .set_index_number(index_number(metadata.node))
        .set_hard_links(metadata.link_count)
        .set_ea_size(0);
        if metadata.kind == NodeKind::Symlink {
            info.set_reparse_tag(reparse::IO_REPARSE_TAG_SYMLINK);
        }
        info
    }

    /// [`Self::file_info`] for an entry that lives in `directory`, which is
    /// what a link needs: whether it is presented as a directory link depends
    /// on what its target is.
    fn entry_info(
        &self,
        inode: u64,
        metadata: &Metadata,
        name: &Name,
        directory: &[String],
    ) -> FileInfo {
        let mut info = self.file_info(metadata, name);
        if metadata.kind == NodeKind::Symlink
            && self.link_points_to_directory(inode, metadata, directory)
        {
            let attributes = info.file_attributes().0 | FileAttributes::DIRECTORY.0;
            info.set_file_attributes(FileAttributes(attributes));
        }
        info
    }

    /// Whether the link `inode` in `directory` resolves to a directory,
    /// following at most [`MAX_LINK_HOPS`] links. Anything unresolvable is a
    /// file link, which is what Windows assumes of a link it cannot follow.
    fn link_points_to_directory(
        &self,
        inode: u64,
        metadata: &Metadata,
        directory: &[String],
    ) -> bool {
        let key = (metadata.node, metadata.revision);
        if let Ok(cache) = self.link_kinds.lock()
            && let Some(known) = cache.get(&key)
        {
            return *known;
        }
        let mut hops = 0;
        let mut current = (inode, directory.to_vec());
        // The reference on each followed link is held until it has been read,
        // and returned before the next hop.
        let mut held: Option<Resolved> = None;
        let answer = loop {
            let target = self.block_on(self.adapter().readlink_async(current.0));
            if let Some(previous) = held.take() {
                self.resolver.release_resolved(&previous);
            }
            let Ok(target) = target else {
                break false;
            };
            let Some(components) = reparse::target_components(&current.1, &target) else {
                break false;
            };
            let Ok(resolved) = self.block_on(self.resolver.resolve(&components)) else {
                break false;
            };
            match resolved.kind() {
                NodeKind::Symlink if hops < MAX_LINK_HOPS => {
                    hops += 1;
                    let mut parent = components;
                    parent.pop();
                    current = (resolved.inode, parent);
                    held = Some(resolved);
                }
                kind => {
                    self.resolver.release_resolved(&resolved);
                    break kind == NodeKind::Directory;
                }
            }
        };
        if let Ok(mut cache) = self.link_kinds.lock() {
            if cache.len() >= MAX_CACHED_LINK_KINDS {
                cache.clear();
            }
            cache.insert(key, answer);
        }
        answer
    }

    /// The reparse buffer for the link `inode` found at `path`.
    fn link_reparse_data(
        &self,
        inode: u64,
        path: &[String],
        buffer: &mut [u8],
    ) -> Result<usize, NtStatus> {
        let target = self
            .block_on(self.adapter().readlink_async(inode))
            .map_err(adapter_status)?;
        let directory = path.split_last().map(|(_, parent)| parent).unwrap_or(&[]);
        let windows = reparse::windows_target(&target, directory)
            .map_err(|_| status::STATUS_IO_REPARSE_DATA_INVALID)?;
        reparse::encode_symlink(&windows, buffer).map_err(|error| match error {
            reparse::ReparseError::BufferTooSmall => status::STATUS_BUFFER_TOO_SMALL,
            _ => status::STATUS_IO_REPARSE_DATA_INVALID,
        })
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
            .block_on(self.adapter().open_async(context.inode(), options))
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
            .block_on(self.adapter().opendir_async(context.inode()))
            .map_err(adapter_status)?;
        let (listing, current, parent) = self
            .adapter()
            .directory_listing_with_metadata(handle, context.inode())
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
    const GET_REPARSE_POINT_DEFINED: bool = true;
    const SET_REPARSE_POINT_DEFINED: bool = true;
    const DELETE_REPARSE_POINT_DEFINED: bool = true;
    const RESOLVE_REPARSE_POINTS_DEFINED: bool = true;
    const GET_STREAM_INFO_DEFINED: bool = true;
    const SET_VOLUME_LABEL_DEFINED: bool = true;
    const SET_SECURITY_DEFINED: bool = true;

    fn get_volume_info(&self) -> Result<VolumeInfo, NtStatus> {
        let statistics = self
            .block_on(self.adapter().statfs_async())
            .map_err(adapter_status)?;
        let block = u64::from(statistics.block_size.max(1));
        let total = statistics.blocks.saturating_mul(block);
        let free = statistics.blocks_available.saturating_mul(block);
        let label = self
            .volume_label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        VolumeInfo::new(total, free, U16Str::from_slice(label.as_slice()))
            .map_err(|_| status::STATUS_INVALID_PARAMETER)
    }

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        find_reparse_point: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool), NtStatus> {
        let target = self.stream_target(file_name)?;
        // Held until the attributes are computed: describing a link means
        // reading it, which needs the adapter to still know its inode.
        let resolved = match self
            .block_on(self.resolver.resolve(&target.components))
            .map_err(adapter_status)
        {
            Ok(resolved) => resolved,
            // The walk stops at a link that is not the last component; WinFsp
            // then reparses the name through it. The value it hands back is the
            // index of that component, which is what WinFsp expects here.
            Err(error) => {
                return match find_reparse_point() {
                    Some(index) => Ok((
                        index,
                        self.security.for_node(0o755, NodeKind::Directory)?,
                        true,
                    )),
                    None => Err(error),
                };
            }
        };
        let name = target
            .components
            .last()
            .map(|name| Name::from(name.clone()))
            .unwrap_or_default();
        // The same attributes every other callback reports, including the
        // directory bit on a link to a directory: WinFsp refuses to open, as a
        // directory, a link this says is a file link, exactly as NTFS refuses
        // to traverse a link made without `mklink /D`.
        let directory = target
            .components
            .split_last()
            .map(|(_, parent)| parent)
            .unwrap_or(&[]);
        let attributes = self
            .entry_info(resolved.inode, &resolved.metadata, &name, directory)
            .file_attributes();
        self.resolver.release_resolved(&resolved);
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
        let target = self.stream_target(file_name)?;
        if let Some(stream) = target.stream {
            let (handle, info) =
                self.create_stream(&target.components, stream, create_file_info)?;
            if let Some(context) = handle.get() {
                context.set_normalized(Some(file_name.to_ucstring()));
            }
            return Ok((handle, info));
        }
        let (parent, name) = self.parent_and_name(file_name, true)?;
        let parent_inode = parent.inode;
        let path = self.components(file_name)?;
        let result = if create_file_info
            .create_options
            .is(CreateOptions::FILE_DIRECTORY_FILE)
        {
            self.create_directory(parent_inode, &name, create_file_info, path)
        } else {
            self.create_file(parent_inode, &name, create_file_info, path)
        };
        self.resolver.release_resolved(&parent);
        let (context, info) = result?;
        context.set_normalized(Some(file_name.to_ucstring()));
        self.set_location(&context, Location { parent_inode, name });
        Ok((Handle::new(context), info))
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), NtStatus> {
        let target = self.stream_target(file_name)?;
        let components = target.components;
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

        let normalized = self.normalized_path(&components, location.as_ref(), resolved.inode);
        if let Some(stream) = target.stream {
            let stream_suffix = format!(":{}", String::from_utf8_lossy(stream.as_bytes()));
            let (handle, info) = self.open_stream(
                resolved,
                location,
                components,
                stream,
                create_options,
                granted_access,
            )?;
            if let Some(context) = handle.get() {
                context.set_normalized(normalized.and_then(|base| {
                    U16CString::from_str(format!("{}{stream_suffix}", base.to_string_lossy())).ok()
                }));
            }
            return Ok((handle, info));
        }

        // Windows asks for a directory or a non-directory explicitly, and an
        // application that opened a directory expecting a file misbehaves in
        // ways that are hard to trace back here.
        // A link is opened as itself here — WinFsp has already followed it
        // unless the caller asked for the link — and a directory link is
        // legitimately opened with either option.
        let is_link = resolved.kind() == NodeKind::Symlink;
        let mismatch = if is_link {
            None
        } else if create_options.is(CreateOptions::FILE_DIRECTORY_FILE) && !resolved.is_directory()
        {
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
        let directory = components
            .split_last()
            .map(|(_, parent)| parent)
            .unwrap_or(&[]);
        let info = self.entry_info(resolved.inode, &resolved.metadata, &name, directory);
        let context = FileContext::new(&resolved, granted_access, location, components.clone());
        context.set_normalized(normalized);
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
        context.modified.store(true, Ordering::Release);
        if let Some(stream) = &context.stream {
            return self.stream_overwrite(&context, stream);
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
                    .setattr_async(context.inode(), Some(handle), changes),
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
        let delete =
            flags.is(CleanupFlags::DELETE) || context.delete_on_close.load(Ordering::Acquire);
        if let Some(stream) = &context.stream {
            // Deleting a stream removes the attribute, never the file.
            self.stream_cleanup(&context, stream, delete);
            return;
        }
        // The last user handle is gone, so whatever the application wrote is,
        // as far as it knows, written. WinFsp defers `Close` — by seconds when
        // the cache manager holds the file — and the adapter otherwise flushes
        // a handle's coalesced bytes only at release. Flushing here makes the
        // data visible to other opens and clients at the moment Windows says
        // the file is closed, and lands it before anything set afterwards (a
        // `SetFileTime` from the next open) instead of overwriting it.
        let handle = context.file.lock().ok().and_then(|slot| *slot);
        if let Some(handle) = handle
            && context.writable()
        {
            let _ = self.block_on(self.adapter().flush_async(handle, None));
        }
        // WinFsp asks for the modification time to be stamped from state it
        // keeps per file, not per open, so the request also arrives on an open
        // that only set an explicit time — and stamping then would overwrite
        // the time the application just set. The server already moves the
        // time on every write; stamp only for an open that changed the file
        // and did not choose a time itself.
        if (flags.is(CleanupFlags::SET_LAST_WRITE_TIME) || flags.is(CleanupFlags::SET_CHANGE_TIME))
            && context.modified.load(Ordering::Acquire)
            && !context.explicit_write_time.load(Ordering::Acquire)
        {
            self.set_modified_now(context.inode());
        }
        if delete && let Some(location) = self.location_of(&context) {
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
        self.release_open_handles(&context);
        self.resolver.release(context.inode());
    }

    fn read(
        &self,
        context: Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> Result<usize, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if let Some(stream) = &context.stream {
            return self.stream_read(&context, stream, buffer, offset);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let handle = self.file_handle(&context)?;
        // Windows passes an application's read through at whatever size it
        // asked for — `ReadFile` of a whole file is common — while one adapter
        // callback is bounded. Fill the buffer in bounded pieces.
        let mut filled = 0;
        while filled < buffer.len() {
            let want = (buffer.len() - filled).min(MAX_IO_PIECE);
            let position = offset
                .checked_add(filled as u64)
                .ok_or(status::STATUS_INVALID_PARAMETER)?;
            let data = match self.block_on(self.adapter().read_async(handle, position, want as u64))
            {
                Ok(data) => data,
                // Bytes already copied are a valid short read; the error
                // surfaces on the caller's next read at the following offset.
                Err(_) if filled > 0 => break,
                Err(error) => return Err(adapter_status(error)),
            };
            let received = data.len().min(want);
            buffer[filled..filled + received].copy_from_slice(&data[..received]);
            filled += received;
            if received < want {
                break;
            }
        }
        if filled == 0 && !buffer.is_empty() {
            // Windows distinguishes "read nothing because the file ended" from
            // "read nothing successfully"; applications loop forever on the
            // latter.
            return Err(status::STATUS_END_OF_FILE);
        }
        Ok(filled)
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
        context.modified.store(true, Ordering::Release);
        if let Some(stream) = &context.stream {
            return self.stream_write(&context, stream, buffer, mode);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let handle = self.file_handle(&context)?;
        let current = self
            .block_on(self.adapter().getattr_async(context.inode()))
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
        let mut written = 0;
        for piece in buffer.chunks(MAX_IO_PIECE) {
            let position = offset
                .checked_add(written as u64)
                .ok_or(status::STATUS_INVALID_PARAMETER)?;
            let landed = match self.block_on(self.adapter().write_async(handle, position, piece)) {
                Ok(landed) => {
                    usize::try_from(landed).map_err(|_| status::STATUS_IO_DEVICE_ERROR)?
                }
                // Report what reached the file; the caller retries the rest.
                Err(_) if written > 0 => break,
                Err(error) => return Err(adapter_status(error)),
            };
            written += landed;
            if landed < piece.len() {
                break;
            }
        }
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode()))
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
        if let Some(stream) = &context.stream {
            self.stream_flush(&context, stream)?;
            return self.stream_info_of(&context, stream);
        }
        if context.is_directory() {
            return self.info_for(context.inode(), &self.name_of(&context));
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
        self.info_for(context.inode(), &self.name_of(&context))
    }

    fn get_file_info(&self, context: Self::FileContext) -> Result<FileInfo, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if let Some(stream) = &context.stream {
            return self.stream_info_of(&context, stream);
        }
        if context.is_symlink() {
            let metadata = self
                .block_on(self.adapter().getattr_async(context.inode()))
                .map_err(adapter_status)?;
            let path = context.path();
            let directory = path.split_last().map(|(_, parent)| parent).unwrap_or(&[]);
            return Ok(self.entry_info(
                context.inode(),
                &metadata,
                &self.name_of(&context),
                directory,
            ));
        }
        self.info_for(context.inode(), &self.name_of(&context))
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
            .block_on(self.adapter().getattr_async(context.inode()))
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
        if changes.modified_unix_ms.is_some() {
            context.explicit_write_time.store(true, Ordering::Release);
        }
        let metadata = self
            .block_on(self.adapter().setattr_async(context.inode(), None, changes))
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
        context.modified.store(true, Ordering::Release);
        if let Some(stream) = &context.stream {
            return self.stream_set_size(&context, stream, new_size, set_allocation_size);
        }
        if context.is_directory() {
            return Err(status::STATUS_FILE_IS_A_DIRECTORY);
        }
        let current = self
            .block_on(self.adapter().getattr_async(context.inode()))
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
            .block_on(
                self.adapter()
                    .setattr_async(context.inode(), handle, changes),
            )
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
        if context.stream.is_some() {
            // NTFS can rename one stream to another; nothing Windows ships
            // relies on it, and an attribute has no rename operation.
            return Err(status::STATUS_INVALID_PARAMETER);
        }
        let source = self
            .location_of(&context)
            .ok_or(status::STATUS_INVALID_HANDLE)?;
        let (destination, new_name) = self.parent_and_name(new_file_name, true)?;
        let new_path = self.components(new_file_name)?;
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
        context.set_path(new_path);
        Ok(())
    }

    fn get_security(&self, context: Self::FileContext) -> Result<PSecurityDescriptor, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode()))
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
        if context.inode() != ROOT_INODE {
            let dot = self.file_info(&current, &Name::from(".".to_owned()));
            if emit(vec![u16::from(b'.')], dot) == Some(false) {
                return Ok(());
            }
            let dotdot = self.file_info(&parent, &Name::from("..".to_owned()));
            if emit(vec![u16::from(b'.'), u16::from(b'.')], dotdot) == Some(false) {
                return Ok(());
            }
        }

        let directory = context.path();
        for entry in &listing.entries {
            // A name the Windows namespace cannot express is skipped rather
            // than mangled: a mangled name would appear in Explorer and then
            // fail to open. This is a documented limitation of the Windows
            // client, not a silent one.
            let Some(name) = path::encode_entry_name(entry.name.as_bytes()) else {
                continue;
            };
            let info = self.entry_info(entry.inode, &entry.metadata, &entry.name, &directory);
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
                    .lookup_async(context.inode(), Name::from(name.clone())),
            )
            .map_err(adapter_status)?;
        let info = self.entry_info(
            result.inode,
            &result.metadata,
            &Name::from(name),
            &context.path(),
        );
        self.resolver.release(result.inode);
        Ok(info)
    }

    fn set_volume_label(&self, volume_label: &U16CStr) -> Result<VolumeInfo, NtStatus> {
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        let label = volume_label
            .to_string()
            .map_err(|_| status::STATUS_INVALID_VOLUME_LABEL)?;
        if !crate::mount::is_usable_volume_name(&label) {
            return Err(status::STATUS_INVALID_VOLUME_LABEL);
        }
        // The label is the export's name on the server, as a Finder rename is
        // on macOS, so the next mount — from either client — shows it too.
        self.block_on(
            self.adapter()
                .set_volume_name_async(Name::from(label.clone())),
        )
        .map_err(adapter_status)?;
        let stored =
            U16CString::from_str(&label).map_err(|_| status::STATUS_INVALID_VOLUME_LABEL)?;
        *self
            .volume_label
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stored;
        self.get_volume_info()
    }

    fn set_security(
        &self,
        context: Self::FileContext,
        security_information: u32,
        modification_descriptor: PSecurityDescriptor,
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        // Only the DACL means anything here. Every node is presented as owned
        // by the mounting account, so an owner or group change has nothing to
        // change; it is accepted rather than refused so ACL-copying tools
        // (`robocopy /COPYALL`, `icacls /restore`) keep working.
        if security_information & DACL_SECURITY_INFORMATION == 0 {
            return Ok(());
        }
        let metadata = self
            .block_on(self.adapter().getattr_async(context.inode()))
            .map_err(adapter_status)?;
        // The supplied DACL replaces the node's, so it alone decides the new
        // owner bits. Reading it needs no call into WinFsp, which keeps this
        // testable on a machine without WinFsp installed.
        let modification = modification_descriptor
            .to_descriptor()
            .ok_or(status::STATUS_INVALID_SECURITY_DESCR)?;
        let granted =
            quickfs_windows_support::effective_access(modification.as_bytes(), &self.security.sid)
                .map_err(|_| status::STATUS_INVALID_SECURITY_DESCR)?;
        let mode = attributes::apply_owner_access(metadata.mode, granted);
        if mode == metadata.mode {
            return Ok(());
        }
        let changes = AttributeChanges {
            size: None,
            mode: Some(mode),
            accessed_unix_ms: None,
            modified_unix_ms: None,
            backup_unix_ms: None,
        };
        self.block_on(self.adapter().setattr_async(context.inode(), None, changes))
            .map_err(adapter_status)?;
        Ok(())
    }

    fn get_stream_info(
        &self,
        context: Self::FileContext,
        buffer: &mut [u8],
    ) -> Result<usize, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        self.list_streams(&context, buffer)
    }

    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: Option<&mut [u8]>,
    ) -> Result<usize, NtStatus> {
        let components = self.components(file_name)?;
        // Held, not transient: a link is not a directory, so releasing it
        // would let the adapter forget the inode before it is read.
        let resolved = self
            .block_on(self.resolver.resolve(&components))
            .map_err(adapter_status)?;
        let result = if resolved.kind() != NodeKind::Symlink {
            Err(status::STATUS_NOT_A_REPARSE_POINT)
        } else {
            match buffer {
                Some(buffer) => self.link_reparse_data(resolved.inode, &components, buffer),
                // WinFsp only asks whether this component is a reparse point.
                None => Ok(0),
            }
        };
        self.resolver.release_resolved(&resolved);
        result
    }

    fn get_reparse_point(
        &self,
        context: Self::FileContext,
        file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> Result<usize, NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !context.is_symlink() {
            return Err(status::STATUS_NOT_A_REPARSE_POINT);
        }
        let path = self
            .components(file_name)
            .unwrap_or_else(|_| context.path());
        self.link_reparse_data(context.inode(), &path, buffer)
    }

    fn set_reparse_point(
        &self,
        context: Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        let target = match reparse::decode_symlink(buffer) {
            Ok((target, relative)) => reparse::posix_target(&target, relative),
            Err(error) => Err(error),
        }
        .map_err(|error| match error {
            reparse::ReparseError::UnsupportedTag => status::STATUS_IO_REPARSE_TAG_NOT_HANDLED,
            reparse::ReparseError::AbsoluteTarget => status::STATUS_ACCESS_DENIED,
            _ => status::STATUS_IO_REPARSE_DATA_INVALID,
        })?;
        let location = self
            .location_of(&context)
            .ok_or(status::STATUS_INVALID_HANDLE)?;
        // `CreateSymbolicLink` creates an empty file or directory first and
        // then sets the link on it. quicKFS stores a link as its own kind of
        // node, so the placeholder is replaced: it must still be empty, which
        // is what makes this safe to do to a node that already existed.
        let current = self
            .block_on(self.adapter().getattr_async(context.inode()))
            .map_err(adapter_status)?;
        match current.kind {
            NodeKind::File if current.size == 0 => {}
            NodeKind::Directory => {
                let (listing, _, _) = self.directory_snapshot(&context)?;
                if !listing.entries.is_empty() {
                    return Err(status::STATUS_DIRECTORY_NOT_EMPTY);
                }
            }
            _ => return Err(status::STATUS_IO_REPARSE_DATA_INVALID),
        }
        self.release_open_handles(&context);
        self.block_on(self.adapter().remove_async(
            location.parent_inode,
            location.name.clone(),
            current.kind == NodeKind::Directory,
        ))
        .map_err(adapter_status)?;
        let created = self
            .block_on(self.adapter().create_symlink_async(
                location.parent_inode,
                location.name,
                target,
            ))
            .map_err(adapter_status)?;
        let previous = context.inode();
        context.replace_identity(created.inode, NodeKind::Symlink);
        self.resolver.release(previous);
        Ok(())
    }

    fn delete_reparse_point(
        &self,
        context: Self::FileContext,
        _file_name: &U16CStr,
        _buffer: &mut [u8],
    ) -> Result<(), NtStatus> {
        let context = context.get().ok_or(status::STATUS_INVALID_HANDLE)?.clone();
        if !self.capabilities.writable {
            return Err(status::STATUS_MEDIA_WRITE_PROTECTED);
        }
        if !context.is_symlink() {
            return Err(status::STATUS_NOT_A_REPARSE_POINT);
        }
        let location = self
            .location_of(&context)
            .ok_or(status::STATUS_INVALID_HANDLE)?;
        // NTFS leaves an ordinary empty file behind when a link's reparse data
        // is deleted; do the same.
        self.block_on(self.adapter().remove_async(
            location.parent_inode,
            location.name.clone(),
            false,
        ))
        .map_err(adapter_status)?;
        let created = self
            .block_on(self.adapter().create_file_async(
                location.parent_inode,
                location.name,
                DEFAULT_FILE_MODE,
                FileOpenOptions {
                    access: FileAccess::ReadOnly,
                    truncate: false,
                    append: false,
                },
            ))
            .map_err(adapter_status)?;
        let _ = self.block_on(self.adapter().release_async(created.handle, false, None));
        let previous = context.inode();
        context.replace_identity(created.inode, NodeKind::File);
        self.resolver.release(previous);
        Ok(())
    }

    const DISPATCHER_STOPPED_DEFINED: bool = true;

    fn normalized_name(&self, file_context: &Self::FileContext) -> Option<U16CString> {
        file_context.get()?.normalized.lock().ok()?.clone()
    }

    fn dispatcher_stopped(&self, _normally: bool) {
        // The volume is going away: return every retained translation and close
        // whatever server handles are still open, so an abrupt unmount does not
        // leave descriptors behind on the server.
        self.resolver.release_all();
        self.adapter().destroy_mount();
    }
}

impl WindowsFilesystem {
    /// The path `components` names, with the final component in its stored
    /// spelling when the parent's listing already knows it.
    fn normalized_path(
        &self,
        components: &[String],
        location: Option<&Location>,
        inode: u64,
    ) -> Option<U16CString> {
        let mut parts: Vec<String> = components.to_vec();
        if let (Some(last), Some(location)) = (parts.last_mut(), location)
            && let Some(stored) = self
                .adapter()
                .stored_entry_name(location.parent_inode, inode)
            && let Ok(stored) = String::from_utf8(stored.as_bytes().to_vec())
        {
            *last = stored;
        }
        U16CString::from_str(format!("\\{}", parts.join("\\"))).ok()
    }

    /// Close whatever server file and directory handles `context` holds.
    fn release_open_handles(&self, context: &FileContext) {
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
    }

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
        path: Vec<String>,
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
        let context = FileContext::new(&resolved, create_file_info.granted_access, None, path);
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
        path: Vec<String>,
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
        let context = FileContext::new(&resolved, create_file_info.granted_access, None, path);
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

/// A 64-bit file ID for a node: its 128-bit identifier folded in half. Zero
/// means "no file ID" to Windows, so it is avoided.
fn index_number(node: NodeId) -> u64 {
    let (high, low) = node.0.as_u64_pair();
    match high ^ low {
        0 => 1,
        folded => folded,
    }
}
