// SPDX-License-Identifier: Apache-2.0

use super::{Adapter, AdapterError, CreatedNode, DirectoryListing};
use fuser::{
    BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, ForgetOne, Generation, INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner,
    MountOption, OpenAccMode, OpenFlags, PollEvents, PollFlags, PollNotifier, RenameFlags,
    ReplyAttr, ReplyBmap, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyIoctl, ReplyLock, ReplyLseek, ReplyOpen, ReplyPoll, ReplyStatfs, ReplyWrite,
    ReplyXTimes, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use quickfs_client_core::ClientError;
use quickfs_protocol::{
    AttributeChanges, ErrorCode, FileAccess, FileLock, FileOpenOptions, LockKind, Metadata, Name,
    NodeKind, RenameMode, SafeIoctl, SeekWhence, SpecialNodeKind, XattrSetMode,
};
use std::{
    ffi::OsStr,
    future::Future,
    io,
    os::unix::ffi::OsStrExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ATTRIBUTE_TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4_096;
const MAX_BACKGROUND_REQUESTS: u16 = 64;
const CONGESTION_THRESHOLD: u16 = 48;
const DARWIN_IOCTL_INOUT: u32 = 0xc000_0000;

const fn darwin_iowr(group: u8, number: u8, size: u32) -> u32 {
    DARWIN_IOCTL_INOUT | ((size & 0x1fff) << 16) | ((group as u32) << 8) | number as u32
}

// Darwin implements lseek(..., SEEK_HOLE/SEEK_DATA) by issuing these private
// vnode ioctls. macFUSE forwards them as FUSE_IOCTL rather than FUSE_LSEEK.
const FSIOC_FIOSEEKHOLE: u32 = darwin_iowr(b'A', 16, std::mem::size_of::<i64>() as u32);
const FSIOC_FIOSEEKDATA: u32 = darwin_iowr(b'A', 17, std::mem::size_of::<i64>() as u32);

#[derive(Clone, Debug)]
pub struct MountConfig {
    pub volume_name: String,
    pub filesystem_name: String,
    pub backend: MacFuseBackend,
}

/// macFUSE transport used to bridge the userspace FUSE protocol into macOS.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MacFuseBackend {
    /// Let macFUSE select its default backend. This remains compatible with
    /// macFUSE 4 and normally uses the kernel extension.
    #[default]
    Automatic,
    /// Use macFUSE 5's FSKit backend, avoiding the legacy kernel extension on
    /// supported macOS releases.
    FsKit,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            volume_name: "quicKFS".into(),
            filesystem_name: "quickfs".into(),
            backend: MacFuseBackend::Automatic,
        }
    }
}

pub fn mount(adapter: Adapter, mountpoint: &Path, config: &MountConfig) -> io::Result<()> {
    validate_mount_config(config)?;
    let capabilities = adapter
        .probe_capabilities()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let volume_name = if config.volume_name == MountConfig::default().volume_name {
        capabilities.volume_name.clone()
    } else {
        config.volume_name.clone()
    };
    let mut fuser_config = Config::default();
    fuser_config.mount_options.extend([
        if capabilities.writable {
            MountOption::RW
        } else {
            MountOption::RO
        },
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::NoAtime,
        MountOption::FSName(config.filesystem_name.clone()),
        MountOption::Subtype("quickfs".into()),
        MountOption::CUSTOM(format!("volname={volume_name}")),
    ]);
    if !capabilities.supports_special_nodes {
        fuser_config.mount_options.push(MountOption::NoDev);
    }
    if let Some(option) = backend_mount_option(config.backend) {
        fuser_config.mount_options.push(option);
    }
    // fuser 0.17 deliberately rejects multiple receive loops on macOS. Every
    // potentially blocking callback below moves its reply into the shared
    // Tokio runtime instead, so the one receive loop remains responsive.
    fuser_config.n_threads = Some(1);
    fuser::mount2(adapter, mountpoint, &fuser_config)
}

fn backend_mount_option(backend: MacFuseBackend) -> Option<MountOption> {
    match backend {
        MacFuseBackend::Automatic => None,
        MacFuseBackend::FsKit => Some(MountOption::CUSTOM("backend=fskit".into())),
    }
}

fn validate_mount_config(config: &MountConfig) -> io::Result<()> {
    for value in [&config.volume_name, &config.filesystem_name] {
        if value.is_empty() || value.len() > 255 || value.contains(',') || value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mount names must contain 1-255 characters and no commas",
            ));
        }
    }
    Ok(())
}

impl Adapter {
    fn spawn_callback(&self, future: impl Future<Output = ()> + Send + 'static) {
        let task = self.runtime().spawn(future);
        drop(task);
    }
}

impl Filesystem for Adapter {
    fn init(&mut self, _request: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let Some(capabilities) = self.cached_capabilities() else {
            return Err(io::Error::other(
                "filesystem capabilities were not loaded before mount",
            ));
        };

        for capability in [
            InitFlags::FUSE_FILE_OPS,
            InitFlags::FUSE_AUTO_INVAL_DATA,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ] {
            add_capability_if_supported(config, capability);
        }
        if capabilities.supports_preallocation {
            add_capability_if_supported(config, InitFlags::FUSE_ALLOCATE);
        }
        if capabilities.supports_exchange_data {
            add_capability_if_supported(config, InitFlags::FUSE_EXCHANGE_DATA);
        }
        if capabilities.supports_atomic_rename {
            // On macOS bits 25/26 are FUSE_CAP_RENAME_SWAP/RENAME_EXCL.
            // fuser 0.17 exposes those bits under their Linux names.
            add_capability_if_supported(config, InitFlags::FUSE_EXPLICIT_INVAL_DATA);
            add_capability_if_supported(config, InitFlags::FUSE_MAP_ALIGNMENT);
        }
        let _ = config.set_max_background(MAX_BACKGROUND_REQUESTS);
        let _ = config.set_congestion_threshold(CONGESTION_THRESHOLD);
        // One FUSE read may be larger than one negotiated wire read; the
        // adapter splits it while preserving the file revision.
        let maximum_readahead = super::MAX_FUSE_IO_SIZE.min(u64::from(u32::MAX));
        if let Ok(maximum_readahead) = u32::try_from(maximum_readahead) {
            let _ = config.set_max_readahead(maximum_readahead);
        }

        if capabilities.writable {
            add_capability_if_supported(config, InitFlags::FUSE_ATOMIC_O_TRUNC);
            add_capability_if_supported(config, InitFlags::FUSE_BIG_WRITES);
            // Keep one kernel write within one wire write so O_APPEND remains
            // atomic across clients. `write_async` still chunks larger direct
            // adapter calls defensively.
            let maximum = capabilities
                .max_write_size
                .min(super::MAX_FUSE_IO_SIZE)
                .min(u64::from(u32::MAX));
            if let Ok(maximum) = u32::try_from(maximum) {
                let _ = config.set_max_write(maximum);
            }
        }
        if capabilities.supports_locks {
            add_capability_if_supported(config, InitFlags::FUSE_POSIX_LOCKS);
        }
        if capabilities.supports_readdirplus {
            add_capability_if_supported(config, InitFlags::FUSE_DO_READDIRPLUS);
            add_capability_if_supported(config, InitFlags::FUSE_READDIRPLUS_AUTO);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        self.destroy_mount();
    }

    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.lookup_async(u64::from(parent), name).await {
                Ok(result) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, result.inode, &result.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn forget(&self, _request: &Request, inode: INodeNo, nlookup: u64) {
        let _ = self.forget_inode(u64::from(inode), nlookup);
    }

    fn batch_forget(&self, _request: &Request, nodes: &[ForgetOne]) {
        let requests = nodes
            .iter()
            .map(|node| (u64::from(node.nodeid()), node.nlookup()))
            .collect::<Vec<_>>();
        let _ = self.forget_inodes(&requests);
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter.getattr_async(inode).await {
                Ok(metadata) => {
                    reply.attr(&ATTRIBUTE_TTL, &file_attr(&adapter, inode, &metadata));
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        handle: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if uid.is_some()
            || gid.is_some()
            || ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some()
            || flags.is_some()
        {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let accessed_unix_ms = match atime.map(time_or_now_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let modified_unix_ms = match mtime.map(time_or_now_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let backup_unix_ms = match bkuptime.map(system_time_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter
                .setattr_async(
                    inode,
                    handle.map(u64::from),
                    AttributeChanges {
                        size,
                        mode: mode.map(|value| value & 0o7777),
                        accessed_unix_ms,
                        modified_unix_ms,
                        backup_unix_ms,
                    },
                )
                .await
            {
                Ok(metadata) => {
                    reply.attr(&ATTRIBUTE_TTL, &file_attr(&adapter, inode, &metadata));
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn readlink(&self, _request: &Request, inode: INodeNo, reply: ReplyData) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.readlink_async(u64::from(inode)).await {
                Ok(target) => reply.data(&target),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn mknod(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let permissions = permission_mode(mode, umask);
        let kind = match mode & u32::from(libc::S_IFMT) {
            value if value == u32::from(libc::S_IFREG) => None,
            value if value == u32::from(libc::S_IFIFO) => Some(SpecialNodeKind::NamedPipe),
            value if value == u32::from(libc::S_IFCHR) => Some(SpecialNodeKind::CharacterDevice),
            value if value == u32::from(libc::S_IFBLK) => Some(SpecialNodeKind::BlockDevice),
            value if value == u32::from(libc::S_IFSOCK) => Some(SpecialNodeKind::Socket),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            let result = if let Some(kind) = kind {
                let device = rdev as libc::dev_t;
                adapter
                    .create_special_node_async(
                        u64::from(parent),
                        name,
                        kind,
                        permissions,
                        u32::try_from(libc::major(device)).unwrap_or_default(),
                        u32::try_from(libc::minor(device)).unwrap_or_default(),
                    )
                    .await
            } else {
                match adapter
                    .create_file_async(
                        u64::from(parent),
                        name,
                        permissions,
                        FileOpenOptions {
                            access: FileAccess::ReadWrite,
                            truncate: false,
                            append: false,
                        },
                    )
                    .await
                {
                    Ok(created) => {
                        let node = CreatedNode {
                            inode: created.inode,
                            metadata: created.metadata,
                        };
                        let _ = adapter.release_async(created.handle, false, None).await;
                        Ok(node)
                    }
                    Err(error) => Err(error),
                }
            };
            match result {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_directory_async(u64::from(parent), name, permission_mode(mode, umask))
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        remove_callback(self, parent, name, false, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        remove_callback(self, parent, name, true, reply);
    }

    fn symlink(
        &self,
        _request: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let link_name = Name::new(link_name.as_bytes().to_vec());
        let target = target.as_os_str().as_bytes().to_vec();
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_symlink_async(u64::from(parent), link_name, target)
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn link(
        &self,
        _request: &Request,
        inode: INodeNo,
        new_parent: INodeNo,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        let new_name = Name::new(new_name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_hard_link_async(u64::from(inode), u64::from(new_parent), new_name)
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mode = match rename_mode(flags) {
            Ok(mode) => mode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let name = Name::new(name.as_bytes().to_vec());
        let new_name = Name::new(new_name.as_bytes().to_vec());
        rename_callback(self, parent, name, new_parent, new_name, mode, reply);
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let options = open_options(flags);
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.open_async(u64::from(inode), options).await {
                Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let options = open_options(OpenFlags(flags));
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_file_async(
                    u64::from(parent),
                    name,
                    permission_mode(mode, umask),
                    options,
                )
                .await
            {
                Ok(created) => reply.created(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                    FileHandle(created.handle),
                    FopenFlags::empty(),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn read(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .read_async(u64::from(handle), offset, u64::from(size))
                .await
            {
                Ok(data) => reply.data(&data),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let data = data.to_vec();
        let synchronize = write_sync_mode(flags);
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.write_async(u64::from(handle), offset, &data).await {
                Ok(written) => {
                    if let Some(data_only) = synchronize
                        && let Err(error) = adapter.fsync_async(u64::from(handle), data_only).await
                    {
                        reply.error(errno(&error));
                        return;
                    }
                    match u32::try_from(written) {
                        Ok(written) => reply.written(written),
                        Err(_) => reply.error(Errno::EOVERFLOW),
                    }
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn flush(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .flush_async(u64::from(handle), Some(lock_owner.0))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .release_async(u64::from(handle), flush, lock_owner.map(|owner| owner.0))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn fsync(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        data_only: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.fsync_async(u64::from(handle), data_only).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn opendir(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EISDIR);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.opendir_async(u64::from(inode)).await {
                Ok(handle) => {
                    reply.opened(FileHandle(handle), FopenFlags::FOPEN_CACHE_DIR);
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.directory_listing(u64::from(handle), u64::from(inode)) {
                Ok(listing) => {
                    fill_directory(&mut reply, u64::from(inode), offset, &listing);
                    reply.ok();
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn readdirplus(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter.directory_listing(u64::from(handle), inode) {
                Ok(listing) => {
                    let current = match adapter.getattr_async(inode).await {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            reply.error(errno(&error));
                            return;
                        }
                    };
                    let parent = match adapter.getattr_async(listing.parent_inode).await {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            reply.error(errno(&error));
                            return;
                        }
                    };
                    fill_directory_plus(
                        &adapter, &mut reply, inode, offset, &listing, &current, &parent,
                    );
                    reply.ok();
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn releasedir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match Adapter::releasedir(self, u64::from(handle)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .fsyncdir_async(u64::from(inode), u64::from(handle))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn statfs(&self, _request: &Request, _inode: INodeNo, reply: ReplyStatfs) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.statfs_async().await {
                Ok(stats) => reply.statfs(
                    stats.blocks,
                    stats.blocks_free,
                    stats.blocks_available,
                    stats.files,
                    stats.files_free,
                    stats.block_size,
                    stats.name_length,
                    stats.fragment_size,
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        let mode = match xattr_set_mode(flags) {
            Ok(mode) => mode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let name = Name::new(name.as_bytes().to_vec());
        let value = value.to_vec();
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .set_xattr_async(u64::from(inode), name, value, mode, position)
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn getxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            if size == 0 {
                match adapter.xattr_size_async(u64::from(inode), name).await {
                    Ok(length) => match u32::try_from(length) {
                        Ok(length) => reply.size(length),
                        Err(_) => reply.error(Errno::E2BIG),
                    },
                    Err(error) => reply.error(errno(&error)),
                }
            } else {
                match adapter.get_xattr_async(u64::from(inode), name).await {
                    Ok(value) if value.len() <= size as usize => reply.data(&value),
                    Ok(_) => reply.error(Errno::ERANGE),
                    Err(error) => reply.error(errno(&error)),
                }
            }
        });
    }

    fn listxattr(&self, _request: &Request, inode: INodeNo, size: u32, reply: ReplyXattr) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.list_xattrs_async(u64::from(inode)).await {
                Ok(names) => {
                    let mut encoded = Vec::new();
                    for name in names {
                        encoded.extend_from_slice(name.as_bytes());
                        encoded.push(0);
                    }
                    if size == 0 {
                        match u32::try_from(encoded.len()) {
                            Ok(length) => reply.size(length),
                            Err(_) => reply.error(Errno::E2BIG),
                        }
                    } else if encoded.len() <= size as usize {
                        reply.data(&encoded);
                    } else {
                        reply.error(Errno::ERANGE);
                    }
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn removexattr(&self, _request: &Request, inode: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.remove_xattr_async(u64::from(inode), name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let lock = match protocol_lock(lock_owner, start, end, typ, pid) {
            Ok(lock) => lock,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.get_lock_async(u64::from(handle), lock).await {
                Ok(Some(conflict)) => reply.locked(
                    conflict.start,
                    conflict.end,
                    lock_type(conflict.kind),
                    conflict.pid,
                ),
                Ok(None) => reply.locked(0, 0, i32::from(libc::F_UNLCK), 0),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let lock = match protocol_lock(lock_owner, start, end, typ, pid) {
            Ok(lock) => lock,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.set_lock_async(u64::from(handle), lock, sleep).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn bmap(
        &self,
        _request: &Request,
        inode: INodeNo,
        block_size: u32,
        block: u64,
        reply: ReplyBmap,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .map_block_async(u64::from(inode), block_size, block)
                .await
            {
                Ok(mapped) => reply.bmap(mapped),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn ioctl(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        flags: IoctlFlags,
        command: u32,
        input: &[u8],
        output_size: u32,
        reply: ReplyIoctl,
    ) {
        if command == FSIOC_FIOSEEKHOLE || command == FSIOC_FIOSEEKDATA {
            if !flags.is_empty()
                || input.len() != std::mem::size_of::<i64>()
                || output_size < std::mem::size_of::<i64>() as u32
            {
                reply.error(Errno::ENOTTY);
                return;
            }
            let Ok(bytes) = <[u8; std::mem::size_of::<i64>()]>::try_from(input) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let offset = i64::from_ne_bytes(bytes);
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let whence = if command == FSIOC_FIOSEEKDATA {
                SeekWhence::Data
            } else {
                SeekWhence::Hole
            };
            let adapter = self.clone();
            self.spawn_callback(async move {
                match adapter.lseek_async(u64::from(handle), offset, whence).await {
                    Ok(offset) => match i64::try_from(offset) {
                        Ok(offset) => reply.ioctl(0, &offset.to_ne_bytes()),
                        Err(_) => reply.error(Errno::EOVERFLOW),
                    },
                    Err(error) => reply.error(errno(&error)),
                }
            });
            return;
        }
        if !flags.is_empty()
            || !input.is_empty()
            || command != libc::FIONREAD as u32
            || output_size < std::mem::size_of::<i32>() as u32
        {
            reply.error(Errno::ENOTTY);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .safe_ioctl_async(u64::from(handle), SafeIoctl::BytesAvailable)
                .await
            {
                Ok(value) => {
                    let value = i32::try_from(value).unwrap_or(i32::MAX);
                    reply.ioctl(0, &value.to_ne_bytes());
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn poll(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _notifier: PollNotifier,
        events: PollEvents,
        _flags: PollFlags,
        reply: ReplyPoll,
    ) {
        match self.poll_events(u64::from(handle), events) {
            Ok(ready) => reply.poll(ready),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn fallocate(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        if mode != 0 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .allocate_async(u64::from(handle), offset, length)
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn lseek(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let Ok(offset) = u64::try_from(offset) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let whence = match whence {
            libc::SEEK_DATA => SeekWhence::Data,
            libc::SEEK_HOLE => SeekWhence::Hole,
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.lseek_async(u64::from(handle), offset, whence).await {
                Ok(offset) => match i64::try_from(offset) {
                    Ok(offset) => reply.offset(offset),
                    Err(_) => reply.error(Errno::EOVERFLOW),
                },
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _request: &Request,
        _input_inode: INodeNo,
        input_handle: FileHandle,
        input_offset: u64,
        _output_inode: INodeNo,
        output_handle: FileHandle,
        output_offset: u64,
        length: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .copy_file_range_async(
                    u64::from(input_handle),
                    input_offset,
                    u64::from(output_handle),
                    output_offset,
                    length,
                )
                .await
            {
                Ok(copied) => match u32::try_from(copied) {
                    Ok(copied) => reply.written(copied),
                    Err(_) => reply.error(Errno::EOVERFLOW),
                },
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setvolname(&self, _request: &Request, name: &OsStr, reply: ReplyEmpty) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.set_volume_name_async(name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn exchange(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        options: u64,
        reply: ReplyEmpty,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let new_name = Name::new(new_name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .exchange_data_async(
                    u64::from(parent),
                    name,
                    u64::from(new_parent),
                    new_name,
                    options,
                )
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn getxtimes(&self, _request: &Request, inode: INodeNo, reply: ReplyXTimes) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.getattr_async(u64::from(inode)).await {
                Ok(metadata) => reply.xtimes(
                    metadata
                        .backup_unix_ms
                        .map(millis_time)
                        .unwrap_or(UNIX_EPOCH),
                    metadata
                        .created_unix_ms
                        .map(millis_time)
                        .unwrap_or_else(|| metadata_time(&metadata)),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }
}

fn add_capability_if_supported(config: &mut KernelConfig, capability: InitFlags) {
    if config.capabilities().contains(capability) {
        let _ = config.add_capabilities(capability);
    }
}

fn remove_callback(
    adapter: &Adapter,
    parent: INodeNo,
    name: &OsStr,
    directory: bool,
    reply: ReplyEmpty,
) {
    let name = Name::new(name.as_bytes().to_vec());
    let owned = adapter.clone();
    adapter.spawn_callback(async move {
        match owned.remove_async(u64::from(parent), name, directory).await {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn rename_callback(
    adapter: &Adapter,
    parent: INodeNo,
    name: Name,
    new_parent: INodeNo,
    new_name: Name,
    mode: RenameMode,
    reply: ReplyEmpty,
) {
    let owned = adapter.clone();
    adapter.spawn_callback(async move {
        match owned
            .rename_async(
                u64::from(parent),
                name,
                u64::from(new_parent),
                new_name,
                mode,
            )
            .await
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    });
}

fn fill_directory(
    reply: &mut ReplyDirectory,
    inode: u64,
    requested_offset: u64,
    listing: &DirectoryListing,
) {
    let mut index = 0_u64;
    if add_directory_row(
        reply,
        requested_offset,
        &mut index,
        inode,
        FileType::Directory,
        OsStr::new("."),
    ) || add_directory_row(
        reply,
        requested_offset,
        &mut index,
        listing.parent_inode,
        FileType::Directory,
        OsStr::new(".."),
    ) {
        return;
    }
    for entry in &listing.entries {
        if add_directory_row(
            reply,
            requested_offset,
            &mut index,
            entry.inode,
            file_type(entry.kind),
            OsStr::from_bytes(entry.name.as_bytes()),
        ) {
            break;
        }
    }
}

fn add_directory_row(
    reply: &mut ReplyDirectory,
    requested_offset: u64,
    index: &mut u64,
    inode: u64,
    kind: FileType,
    name: &OsStr,
) -> bool {
    let current = *index;
    *index = (*index).saturating_add(1);
    current >= requested_offset && reply.add(INodeNo(inode), *index, kind, name)
}

#[allow(clippy::too_many_arguments)]
fn fill_directory_plus(
    adapter: &Adapter,
    reply: &mut ReplyDirectoryPlus,
    inode: u64,
    requested_offset: u64,
    listing: &DirectoryListing,
    current: &Metadata,
    parent: &Metadata,
) {
    let mut index = 0_u64;
    let current_eligible = index >= requested_offset;
    if add_directory_plus_row(
        reply,
        requested_offset,
        &mut index,
        inode,
        OsStr::new("."),
        &file_attr(adapter, inode, current),
    ) {
        return;
    }
    if current_eligible {
        let _ = adapter.add_lookup(inode, 1);
    }
    let parent_eligible = index >= requested_offset;
    if add_directory_plus_row(
        reply,
        requested_offset,
        &mut index,
        listing.parent_inode,
        OsStr::new(".."),
        &file_attr(adapter, listing.parent_inode, parent),
    ) {
        return;
    }
    if parent_eligible {
        let _ = adapter.add_lookup(listing.parent_inode, 1);
    }
    for entry in &listing.entries {
        let eligible = index >= requested_offset;
        let entry_inode = adapter
            .remember_entry(entry.metadata.node, inode, &entry.name)
            .unwrap_or(entry.inode);
        let full = add_directory_plus_row(
            reply,
            requested_offset,
            &mut index,
            entry_inode,
            OsStr::from_bytes(entry.name.as_bytes()),
            &file_attr(adapter, entry_inode, &entry.metadata),
        );
        if full {
            break;
        }
        if eligible {
            let _ = adapter.add_lookup(entry_inode, 1);
        }
    }
}

fn add_directory_plus_row(
    reply: &mut ReplyDirectoryPlus,
    requested_offset: u64,
    index: &mut u64,
    inode: u64,
    name: &OsStr,
    attr: &FileAttr,
) -> bool {
    let current = *index;
    *index = index.saturating_add(1);
    current >= requested_offset
        && reply.add(
            INodeNo(inode),
            *index,
            name,
            &ATTRIBUTE_TTL,
            attr,
            Generation(0),
        )
}

fn open_options(flags: OpenFlags) -> FileOpenOptions {
    let access = match flags.acc_mode() {
        OpenAccMode::O_RDONLY => FileAccess::ReadOnly,
        OpenAccMode::O_WRONLY => FileAccess::WriteOnly,
        OpenAccMode::O_RDWR => FileAccess::ReadWrite,
    };
    FileOpenOptions {
        access,
        truncate: flags.0 & libc::O_TRUNC != 0,
        append: flags.0 & libc::O_APPEND != 0,
    }
}

fn rename_mode(flags: RenameFlags) -> Result<RenameMode, Errno> {
    match flags.bits() {
        0 => Ok(RenameMode::Replace),
        value if value == libc::RENAME_SWAP => Ok(RenameMode::Exchange),
        value if value == libc::RENAME_EXCL => Ok(RenameMode::NoReplace),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

fn write_sync_mode(flags: OpenFlags) -> Option<bool> {
    if flags.0 & libc::O_SYNC != 0 {
        Some(false)
    } else if flags.0 & libc::O_DSYNC != 0 {
        Some(true)
    } else {
        None
    }
}

fn permission_mode(mode: u32, umask: u32) -> u32 {
    mode & !umask & 0o7777
}

fn time_or_now_millis(value: TimeOrNow) -> Result<u64, Errno> {
    let time = match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    let duration = time.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
    u64::try_from(duration.as_millis()).map_err(|_| Errno::EOVERFLOW)
}

fn system_time_millis(time: SystemTime) -> Result<u64, Errno> {
    let duration = time.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
    u64::try_from(duration.as_millis()).map_err(|_| Errno::EOVERFLOW)
}

fn xattr_set_mode(flags: i32) -> Result<XattrSetMode, Errno> {
    match flags {
        0 => Ok(XattrSetMode::Upsert),
        value if value == libc::XATTR_CREATE => Ok(XattrSetMode::Create),
        value if value == libc::XATTR_REPLACE => Ok(XattrSetMode::Replace),
        _ => Err(Errno::EINVAL),
    }
}

fn protocol_lock(
    owner: LockOwner,
    start: u64,
    end: u64,
    typ: i32,
    pid: u32,
) -> Result<FileLock, Errno> {
    let kind = match typ {
        value if value == i32::from(libc::F_RDLCK) => LockKind::Read,
        value if value == i32::from(libc::F_WRLCK) => LockKind::Write,
        value if value == i32::from(libc::F_UNLCK) => LockKind::Unlock,
        _ => return Err(Errno::EINVAL),
    };
    if end < start {
        return Err(Errno::EINVAL);
    }
    Ok(FileLock {
        owner: owner.0,
        start,
        end,
        kind,
        pid,
    })
}

fn lock_type(kind: LockKind) -> i32 {
    match kind {
        LockKind::Read => i32::from(libc::F_RDLCK),
        LockKind::Write => i32::from(libc::F_WRLCK),
        LockKind::Unlock => i32::from(libc::F_UNLCK),
    }
}

fn file_attr(adapter: &Adapter, inode: u64, metadata: &Metadata) -> FileAttr {
    let kind = file_type(metadata.kind);
    let permission = u16::try_from(metadata.mode & 0o7777).unwrap_or_default();
    let accessed = millis_time(metadata.accessed_unix_ms);
    let modified = metadata_time(metadata);
    let created = metadata
        .created_unix_ms
        .map(millis_time)
        .unwrap_or(UNIX_EPOCH);
    FileAttr {
        ino: INodeNo(inode),
        size: metadata.size,
        blocks: metadata.allocated_blocks,
        atime: accessed,
        mtime: modified,
        // The protocol does not yet carry a distinct inode-change timestamp.
        ctime: modified,
        crtime: created,
        kind,
        perm: permission,
        nlink: metadata.link_count,
        uid: adapter.owner_uid(),
        gid: adapter.owner_gid(),
        rdev: libc::makedev(metadata.device_major as _, metadata.device_minor as _) as u32,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn metadata_time(metadata: &Metadata) -> SystemTime {
    millis_time(metadata.modified_unix_ms)
}

fn millis_time(milliseconds: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap_or(UNIX_EPOCH)
}

fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
        NodeKind::NamedPipe => FileType::NamedPipe,
        NodeKind::CharacterDevice => FileType::CharDevice,
        NodeKind::BlockDevice => FileType::BlockDevice,
        NodeKind::Socket => FileType::Socket,
    }
}

fn errno(error: &AdapterError) -> Errno {
    match error {
        AdapterError::CallbackTimedOut => Errno::ETIMEDOUT,
        AdapterError::UnknownInode => Errno::ESTALE,
        AdapterError::UnknownHandle | AdapterError::UnknownDirectoryHandle => Errno::EBADF,
        AdapterError::HandleInodeMismatch => Errno::EBADF,
        AdapterError::NotFound => Errno::ENOENT,
        AdapterError::InvalidName | AdapterError::InvalidRange => Errno::EINVAL,
        AdapterError::InvalidRemoteName
        | AdapterError::AmbiguousName
        | AdapterError::Runtime(_)
        | AdapterError::StateUnavailable
        | AdapterError::InvalidCapabilities => Errno::EIO,
        AdapterError::InodeSpaceExhausted => Errno::ENFILE,
        AdapterError::HandleSpaceExhausted => Errno::EMFILE,
        AdapterError::UnexpectedMetadata
        | AdapterError::UnexpectedReadLength
        | AdapterError::UnexpectedWriteLength => Errno::EPROTO,
        AdapterError::StaleRevision => Errno::ESTALE,
        AdapterError::RequestTooLarge(_) => Errno::EFBIG,
        AdapterError::ReadOnly => Errno::EROFS,
        AdapterError::Unsupported => Errno::EOPNOTSUPP,
        AdapterError::InvalidAccess => Errno::EBADF,
        AdapterError::Client(client) => client_errno(client),
    }
}

fn client_errno(error: &ClientError) -> Errno {
    match error {
        ClientError::Transport(_) => Errno::EIO,
        ClientError::UnexpectedResponse => Errno::EPROTO,
        ClientError::ReadTooLarge(_) | ClientError::WriteTooLarge(_) => Errno::EFBIG,
        ClientError::StaleRevision => Errno::ESTALE,
        ClientError::Offline | ClientError::OfflineCacheMiss => Errno::ENETDOWN,
        ClientError::AmbiguousMutation => Errno::EIO,
        ClientError::Server(code, _) => match code {
            ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => Errno::EACCES,
            ErrorCode::NotFound => Errno::ENOENT,
            ErrorCode::AlreadyExists => Errno::EEXIST,
            ErrorCode::NotDirectory => Errno::ENOTDIR,
            ErrorCode::IsDirectory => Errno::EISDIR,
            ErrorCode::NotEmpty => Errno::ENOTEMPTY,
            ErrorCode::NoAttribute => Errno::NO_XATTR,
            ErrorCode::NoData => Errno::ENXIO,
            ErrorCode::NotTty => Errno::ENOTTY,
            ErrorCode::ReadOnly => Errno::EROFS,
            ErrorCode::Conflict => Errno::ESTALE,
            ErrorCode::WouldBlock => Errno::EAGAIN,
            ErrorCode::NoSpace => Errno::ENOSPC,
            ErrorCode::Busy => Errno::EBUSY,
            ErrorCode::NotSupported => Errno::EOPNOTSUPP,
            ErrorCode::Offline => Errno::ENETDOWN,
            ErrorCode::InvalidNode => Errno::ESTALE,
            ErrorCode::InvalidHandle => Errno::EBADF,
            ErrorCode::InvalidRequest => Errno::EINVAL,
            ErrorCode::UnsupportedVersion => Errno::EPROTO,
            ErrorCode::TooLarge => Errno::EFBIG,
            ErrorCode::Timeout => Errno::ETIMEDOUT,
            ErrorCode::Internal => Errno::EIO,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_macfuse_fskit_backend_without_changing_the_default() {
        assert_eq!(MountConfig::default().backend, MacFuseBackend::Automatic);
        assert!(backend_mount_option(MacFuseBackend::Automatic).is_none());
        assert!(matches!(
            backend_mount_option(MacFuseBackend::FsKit),
            Some(MountOption::CUSTOM(option)) if option == "backend=fskit"
        ));
    }

    #[test]
    fn converts_open_flags_without_losing_append_or_truncate() {
        let options = open_options(OpenFlags(libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC));
        assert_eq!(options.access, FileAccess::ReadWrite);
        assert!(options.append);
        assert!(options.truncate);
    }

    #[test]
    fn converts_posix_lock_types_and_rejects_invalid_ranges() {
        let lock = protocol_lock(LockOwner(8), 10, 20, i32::from(libc::F_WRLCK), 42).unwrap();
        assert_eq!(lock.owner, 8);
        assert_eq!(lock.kind, LockKind::Write);
        assert!(protocol_lock(LockOwner(8), 20, 10, i32::from(libc::F_WRLCK), 42).is_err());
        assert!(protocol_lock(LockOwner(8), 0, 1, -99, 42).is_err());
    }

    #[test]
    fn applies_umask_and_strips_file_type_bits() {
        assert_eq!(
            permission_mode(u32::from(libc::S_IFREG | 0o666), 0o027),
            0o640
        );
    }

    #[test]
    fn maps_synchronous_write_flags_to_full_or_data_only_sync() {
        assert_eq!(write_sync_mode(OpenFlags(libc::O_SYNC)), Some(false));
        assert_eq!(write_sync_mode(OpenFlags(libc::O_DSYNC)), Some(true));
        assert_eq!(write_sync_mode(OpenFlags(libc::O_RDWR)), None);
    }

    #[test]
    fn maps_macos_renamex_flags_without_confusing_data_exchange() {
        assert_eq!(
            rename_mode(RenameFlags::empty()).unwrap(),
            RenameMode::Replace
        );
        assert_eq!(
            rename_mode(RenameFlags::from_bits_retain(libc::RENAME_SWAP)).unwrap(),
            RenameMode::Exchange
        );
        assert_eq!(
            rename_mode(RenameFlags::from_bits_retain(libc::RENAME_EXCL)).unwrap(),
            RenameMode::NoReplace
        );
        assert!(rename_mode(RenameFlags::from_bits_retain(u32::MAX)).is_err());
    }

    #[test]
    fn encodes_darwin_sparse_seek_ioctls_with_an_off_t_payload() {
        assert_eq!(FSIOC_FIOSEEKHOLE, 0xc008_4110);
        assert_eq!(FSIOC_FIOSEEKDATA, 0xc008_4111);
    }
}
