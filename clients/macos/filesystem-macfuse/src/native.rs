// SPDX-License-Identifier: Apache-2.0

use super::{Adapter, AdapterError};
use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use quickfs_client_core::ClientError;
use quickfs_protocol::{ErrorCode, Metadata, NodeKind};
use std::{
    ffi::OsStr,
    io,
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

const ATTRIBUTE_TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4_096;

#[derive(Clone, Debug)]
pub struct MountConfig {
    pub volume_name: String,
    pub filesystem_name: String,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            volume_name: "quicKFS".into(),
            filesystem_name: "quickfs".into(),
        }
    }
}

pub fn mount(adapter: Adapter, mountpoint: &Path, config: &MountConfig) -> io::Result<()> {
    validate_mount_config(config)?;
    let mut fuser_config = Config::default();
    fuser_config.mount_options.extend([
        MountOption::RO,
        MountOption::DefaultPermissions,
        MountOption::NoDev,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::NoAtime,
        MountOption::FSName(config.filesystem_name.clone()),
        MountOption::Subtype("quickfs".into()),
        MountOption::CUSTOM(format!("volname={}", config.volume_name)),
        MountOption::CUSTOM("noappledouble".into()),
        MountOption::CUSTOM("noapplexattr".into()),
    ]);
    // fuser 0.17 supports multiple event-loop threads only on Linux. Keep the
    // macFUSE loop single-threaded; remote futures still run on the one shared
    // multi-thread Tokio runtime owned by the adapter.
    fuser_config.n_threads = Some(1);
    fuser::mount2(adapter, mountpoint, &fuser_config)
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

impl Filesystem for Adapter {
    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EILSEQ);
            return;
        };
        match Adapter::lookup(self, u64::from(parent), name) {
            Ok(result) => reply.entry(
                &ATTRIBUTE_TTL,
                &file_attr(self, result.inode, &result.metadata),
                Generation(0),
            ),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        let inode = u64::from(inode);
        match Adapter::getattr(self, inode) {
            Ok(metadata) => reply.attr(&ATTRIBUTE_TTL, &file_attr(self, inode, &metadata)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inode = u64::from(inode);
        let listing = match Adapter::readdir(self, inode) {
            Ok(listing) => listing,
            Err(error) => {
                reply.error(errno(&error));
                return;
            }
        };

        let mut index = 0_u64;
        if add_directory_row(
            &mut reply,
            offset,
            &mut index,
            inode,
            FileType::Directory,
            ".",
        ) || add_directory_row(
            &mut reply,
            offset,
            &mut index,
            listing.parent_inode,
            FileType::Directory,
            "..",
        ) {
            reply.ok();
            return;
        }
        for entry in &listing.entries {
            if add_directory_row(
                &mut reply,
                offset,
                &mut index,
                entry.inode,
                file_type(entry.kind),
                &entry.name,
            ) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        match Adapter::open(self, u64::from(inode)) {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(errno(&error)),
        }
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
        match Adapter::read(self, u64::from(handle), offset, u64::from(size)) {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match Adapter::release(self, u64::from(handle)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn flush(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        // There is no local write buffer in this read-only adapter.
        reply.ok();
    }

    fn fsync(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }
        match Adapter::getattr(self, u64::from(inode)) {
            Ok(metadata) if metadata.kind == NodeKind::Directory => {
                reply.opened(FileHandle(0), FopenFlags::empty());
            }
            Ok(_) => reply.error(Errno::ENOTDIR),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn releasedir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn getxattr(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _request: &Request, _inode: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

fn add_directory_row(
    reply: &mut ReplyDirectory,
    requested_offset: u64,
    index: &mut u64,
    inode: u64,
    kind: FileType,
    name: &str,
) -> bool {
    let current = *index;
    *index = (*index).saturating_add(1);
    current >= requested_offset && reply.add(INodeNo(inode), *index, kind, name)
}

fn file_attr(adapter: &Adapter, inode: u64, metadata: &Metadata) -> FileAttr {
    let timestamp = UNIX_EPOCH
        .checked_add(Duration::from_millis(metadata.modified_unix_ms))
        .unwrap_or(UNIX_EPOCH);
    let kind = file_type(metadata.kind);
    FileAttr {
        ino: INodeNo(inode),
        size: metadata.size,
        blocks: metadata.size.div_ceil(512),
        atime: timestamp,
        mtime: timestamp,
        ctime: timestamp,
        crtime: timestamp,
        kind,
        perm: if metadata.kind == NodeKind::Directory {
            0o555
        } else {
            0o444
        },
        nlink: if metadata.kind == NodeKind::Directory {
            2
        } else {
            1
        },
        uid: adapter.owner_uid,
        gid: adapter.owner_gid,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
    }
}

fn errno(error: &AdapterError) -> Errno {
    match error {
        AdapterError::CallbackTimedOut => Errno::ETIMEDOUT,
        AdapterError::UnknownInode => Errno::ESTALE,
        AdapterError::UnknownHandle => Errno::EBADF,
        AdapterError::NotFound => Errno::ENOENT,
        AdapterError::InvalidName => Errno::EINVAL,
        AdapterError::InvalidRemoteName
        | AdapterError::Runtime(_)
        | AdapterError::StateUnavailable => Errno::EIO,
        AdapterError::InodeSpaceExhausted => Errno::ENFILE,
        AdapterError::HandleSpaceExhausted => Errno::EMFILE,
        AdapterError::UnexpectedMetadata | AdapterError::UnexpectedReadLength => Errno::EPROTO,
        AdapterError::ReadTooLarge(_) => Errno::EFBIG,
        AdapterError::Client(client) => client_errno(client),
    }
}

fn client_errno(error: &ClientError) -> Errno {
    match error {
        ClientError::Transport(_) => Errno::EIO,
        ClientError::UnexpectedResponse => Errno::EPROTO,
        ClientError::ReadTooLarge(_) => Errno::EFBIG,
        ClientError::Server(code, _) => match code {
            ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => Errno::EACCES,
            ErrorCode::NotFound => Errno::ENOENT,
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
