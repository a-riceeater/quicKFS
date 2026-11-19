use std::ops::{BitOr, BitOrAssign};
use windows_sys::{
    Wdk::Storage::FileSystem::{
        FILE_COMPLETE_IF_OPLOCKED, FILE_CREATE_TREE_CONNECTION, FILE_DELETE_ON_CLOSE,
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_NO_EA_KNOWLEDGE,
        FILE_NO_INTERMEDIATE_BUFFERING, FILE_OPEN_BY_FILE_ID, FILE_OPEN_FOR_BACKUP_INTENT,
        FILE_OPEN_REPARSE_POINT, FILE_OPEN_REQUIRING_OPLOCK, FILE_RANDOM_ACCESS,
        FILE_RESERVE_OPFILTER, FILE_SEQUENTIAL_ONLY, FILE_SYNCHRONOUS_IO_ALERT,
        FILE_SYNCHRONOUS_IO_NONALERT, FILE_WRITE_THROUGH,
    },
    Win32::Storage::FileSystem::{
        CREATE_ALWAYS, CREATE_NEW, DELETE, FILE_ACCESS_RIGHTS, FILE_ADD_FILE,
        FILE_ADD_SUBDIRECTORY, FILE_ALL_ACCESS, FILE_APPEND_DATA, FILE_ATTRIBUTE_ARCHIVE,
        FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DEVICE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_EA, FILE_ATTRIBUTE_ENCRYPTED, FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_INTEGRITY_STREAM, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_NO_SCRUB_DATA, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_PINNED,
        FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY, FILE_ATTRIBUTE_UNPINNED,
        FILE_ATTRIBUTE_VIRTUAL, FILE_CREATE_PIPE_INSTANCE, FILE_DELETE_CHILD, FILE_EXECUTE,
        FILE_FLAGS_AND_ATTRIBUTES, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_READ_EA, FILE_SHARE_DELETE,
        FILE_SHARE_MODE, FILE_SHARE_NONE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TRAVERSE,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, INVALID_FILE_ATTRIBUTES,
        OPEN_ALWAYS, OPEN_EXISTING, READ_CONTROL, SPECIFIC_RIGHTS_ALL, STANDARD_RIGHTS_ALL,
        STANDARD_RIGHTS_EXECUTE, STANDARD_RIGHTS_READ, STANDARD_RIGHTS_REQUIRED,
        STANDARD_RIGHTS_WRITE, SYNCHRONIZE, TRUNCATE_EXISTING, WRITE_DAC, WRITE_OWNER,
    },
};
use winfsp_wrs_sys::{
    FspCleanupDelete, FspCleanupSetAllocationSize, FspCleanupSetArchiveBit,
    FspCleanupSetChangeTime, FspCleanupSetLastAccessTime, FspCleanupSetLastWriteTime,
};

macro_rules! impl_debug_flags {
    ($name:ident) => {
        impl std::fmt::Debug for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_tuple(stringify!($name))
                    .field(&format_args!("0x{:X}", self.0))
                    .finish()
            }
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
/// File attributes are metadata values stored by the file system on disk and
/// are used by the system and are available to developers via various file I/O
/// APIs.
pub struct FileAttributes(pub FILE_FLAGS_AND_ATTRIBUTES);

impl_debug_flags!(FileAttributes);

// Documentation taken from https://learn.microsoft.com/en-us/windows/win32/fileio/file-attribute-constants
impl FileAttributes {
    /// A file that is read-only. Applications can read the file, but cannot write
    /// to it or delete it. This attribute is not honored on directories. For more
    /// information, see You cannot view or change the Read-only or the System
    /// attributes of folders in Windows Server 2003, in Windows XP, in Windows
    /// Vista or in Windows 7.
    pub const READONLY: Self = Self(FILE_ATTRIBUTE_READONLY);

    /// The file or directory is hidden. It is not included in an ordinary
    /// directory listing.
    pub const HIDDEN: Self = Self(FILE_ATTRIBUTE_HIDDEN);

    /// A file or directory that the operating system uses a part of, or uses
    /// exclusively.
    pub const SYSTEM: Self = Self(FILE_ATTRIBUTE_SYSTEM);

    /// The handle that identifies a directory.
    pub const DIRECTORY: Self = Self(FILE_ATTRIBUTE_DIRECTORY);

    /// A file or directory that is an archive file or directory. Applications
    /// typically use this attribute to mark files for backup or removal.
    pub const ARCHIVE: Self = Self(FILE_ATTRIBUTE_ARCHIVE);

    /// This value is reserved for system use.
    pub const DEVICE: Self = Self(FILE_ATTRIBUTE_DEVICE);

    /// A file that does not have other attributes set. This attribute is valid
    /// only when used alone.
    pub const NORMAL: Self = Self(FILE_ATTRIBUTE_NORMAL);

    /// A file that is being used for temporary storage. File systems avoid writing
    /// data back to mass storage if sufficient cache memory is available, because
    /// typically, an application deletes a temporary file after the handle is
    /// closed. In that scenario, the system can entirely avoid writing the data.
    /// Otherwise, the data is written after the handle is closed.
    pub const TEMPORARY: Self = Self(FILE_ATTRIBUTE_TEMPORARY);

    /// A file that is a sparse file.
    pub const SPARSE_FILE: Self = Self(FILE_ATTRIBUTE_SPARSE_FILE);

    /// A file or directory that has an associated reparse point, or a file that is
    /// a symbolic link.
    pub const REPARSE_POINT: Self = Self(FILE_ATTRIBUTE_REPARSE_POINT);

    /// A file or directory that is compressed. For a file, all of the data in the
    /// file is compressed. For a directory, compression is the default for newly
    /// created files and subdirectories.
    pub const COMPRESSED: Self = Self(FILE_ATTRIBUTE_COMPRESSED);

    /// The data of a file is not available immediately. This attribute indicates
    /// that the file data is physically moved to offline storage. This attribute
    /// is used by Remote Storage, which is the hierarchical storage management
    /// software. Applications should not arbitrarily change this attribute.
    pub const OFFLINE: Self = Self(FILE_ATTRIBUTE_OFFLINE);

    /// The file or directory is not to be indexed by the content indexing service.
    pub const NOT_CONTENT_INDEXED: Self = Self(FILE_ATTRIBUTE_NOT_CONTENT_INDEXED);

    /// A file or directory that is encrypted. For a file, all data streams in the
    /// file are encrypted. For a directory, encryption is the default for newly
    /// created files and subdirectories.
    pub const ENCRYPTED: Self = Self(FILE_ATTRIBUTE_ENCRYPTED);

    /// The directory or user data stream is configured with integrity (only
    /// supported on ReFS volumes). It is not included in an ordinary directory
    /// listing. The integrity setting persists with the file if it's renamed. If a
    ///  file is copied the destination file will have integrity set if either the
    /// source file or destination directory have integrity set.
    /// Windows Server 2008 R2, Windows 7, Windows Server 2008, Windows Vista,
    /// Windows Server 2003 and Windows XP: This flag is not supported until
    /// Windows Server 2012.
    pub const INTEGRITY_STREAM: Self = Self(FILE_ATTRIBUTE_INTEGRITY_STREAM);

    /// This value is reserved for system use.
    pub const VIRTUAL: Self = Self(FILE_ATTRIBUTE_VIRTUAL);

    /// The user data stream not to be read by the background data integrity
    /// scanner (AKA scrubber). When set on a directory it only provides
    /// inheritance. This flag is only supported on Storage Spaces and ReFS
    /// volumes. It is not included in an ordinary directory listing.
    /// Windows Server 2008 R2, Windows 7, Windows Server 2008, Windows Vista,
    /// Windows Server 2003 and Windows XP: This flag is not supported until
    /// Windows 8 and Windows Server 2012.
    pub const NO_SCRUB_DATA: Self = Self(FILE_ATTRIBUTE_NO_SCRUB_DATA);

    /// A file or directory with extended attributes.
    pub const EA: Self = Self(FILE_ATTRIBUTE_EA);

    /// This attribute indicates user intent that the file or directory should be
    /// kept fully present locally even when not being actively accessed. This
    /// attribute is for use with hierarchical storage management software.
    pub const PINNED: Self = Self(FILE_ATTRIBUTE_PINNED);

    /// This attribute indicates that the file or directory should not be kept
    /// fully present locally except when being actively accessed. This attribute
    /// is for use with hierarchical storage management software.
    pub const UNPINNED: Self = Self(FILE_ATTRIBUTE_UNPINNED);

    /// This attribute only appears in directory enumeration classes
    /// (FILE_DIRECTORY_INFORMATION, FILE_BOTH_DIR_INFORMATION, etc.). When this
    /// attribute is set, it means that the file or directory has no physical
    /// representation on the local system; the item is virtual. Opening the item
    /// will be more expensive than normal, e.g. it will cause at least some of it
    /// to be fetched from a remote store.
    pub const RECALL_ON_OPEN: Self = Self(FILE_ATTRIBUTE_RECALL_ON_OPEN);

    /// When this attribute is set, it means that the file or directory is not
    /// fully present locally. For a file that means that not all of its data is on
    /// local storage (e.g. it may be sparse with some data still in remote
    /// storage). For a directory it means that some of the directory contents are
    /// being virtualized from another location. Reading the file / enumerating the
    /// directory will be more expensive than normal, e.g. it will cause at least
    /// some of the file/directory content to be fetched from a remote store. Only
    /// kernel-mode callers can set this bit.
    ///
    /// File system mini filters below the 180000 – 189999 altitude range (FSFilter
    /// HSM Load Order Group) must not issue targeted cached reads or writes to
    /// files that have this attribute set. This could lead to cache pollution and
    /// potential file corruption. For more information, see Handling placeholders.
    pub const RECALL_ON_DATA_ACCESS: Self = Self(FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS);

    pub const INVALID: Self = Self(INVALID_FILE_ATTRIBUTES);

    pub const fn is(self, rhs: Self) -> bool {
        self.0 & rhs.0 == rhs.0
    }
}

impl BitOr for FileAttributes {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FileAttributes {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreateOptions(pub u32);

impl_debug_flags!(CreateOptions);

// Documentation taken from https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile#parameters
impl CreateOptions {
    /// The file being created or opened is a directory file. With this flag, the
    /// CreateDisposition parameter must be set to FILE_CREATE, FILE_OPEN, or
    /// FILE_OPEN_IF. With this flag, other compatible CreateOptions flags include
    /// only the following: FILE_SYNCHRONOUS_IO_ALERT, FILE_SYNCHRONOUS_IO
    /// _NONALERT, FILE_WRITE_THROUGH, FILE_OPEN_FOR_BACKUP_INTENT, and
    /// FILE_OPEN_BY_FILE_ID.
    pub const FILE_DIRECTORY_FILE: CreateOptions = CreateOptions(FILE_DIRECTORY_FILE);

    /// The file being opened must not be a directory file or this call fails. The
    /// file object being opened can represent a data file, a logical, virtual, or
    /// physical device, or a volume.
    pub const FILE_NON_DIRECTORY_FILE: CreateOptions = CreateOptions(FILE_NON_DIRECTORY_FILE);

    /// Applications that write data to the file must actually transfer the data
    /// into the file before any requested write operation is considered complete.
    /// This flag is automatically set if the CreateOptions flag
    /// FILE_NO_INTERMEDIATE _BUFFERING is set.
    pub const FILE_WRITE_THROUGH: CreateOptions = CreateOptions(FILE_WRITE_THROUGH);

    /// All accesses to the file are sequential.
    pub const FILE_SEQUENTIAL_ONLY: CreateOptions = CreateOptions(FILE_SEQUENTIAL_ONLY);

    /// Accesses to the file can be random, so no sequential read-ahead operations
    /// should be performed on the file by FSDs or the system.
    pub const FILE_RANDOM_ACCESS: CreateOptions = CreateOptions(FILE_RANDOM_ACCESS);

    /// The file cannot be cached or buffered in a driver's internal buffers. This
    /// flag is incompatible with the DesiredAccess FILE_APPEND_DATA flag.
    pub const FILE_NO_INTERMEDIATE_BUFFERING: CreateOptions =
        CreateOptions(FILE_NO_INTERMEDIATE_BUFFERING);

    /// All operations on the file are performed synchronously. Any wait on behalf
    /// of the caller is subject to premature termination from alerts. This flag
    /// also causes the I/O system to maintain the file position context. If this
    /// flag is set, the DesiredAccess SYNCHRONIZE flag also must be set.
    pub const FILE_SYNCHRONOUS_IO_ALERT: CreateOptions = CreateOptions(FILE_SYNCHRONOUS_IO_ALERT);

    /// All operations on the file are performed synchronously. Waits in the system
    /// to synchronize I/O queuing and completion are not subject to alerts. This
    /// flag also causes the I/O system to maintain the file position context. If
    /// this flag is set, the DesiredAccess SYNCHRONIZE flag also must be set.
    pub const FILE_SYNCHRONOUS_IO_NONALERT: CreateOptions =
        CreateOptions(FILE_SYNCHRONOUS_IO_NONALERT);

    /// Create a tree connection for this file in order to open it over the network.
    /// This flag is not used by device and intermediate drivers.
    pub const FILE_CREATE_TREE_CONNECTION: CreateOptions =
        CreateOptions(FILE_CREATE_TREE_CONNECTION);

    /// If the extended attributes on an existing file being opened indicate that
    /// the caller must understand EAs to properly interpret the file, fail this
    /// request because the caller does not understand how to deal with EAs. This
    /// flag is irrelevant for device and intermediate drivers.
    pub const FILE_NO_EA_KNOWLEDGE: CreateOptions = CreateOptions(FILE_NO_EA_KNOWLEDGE);

    /// Open a file with a reparse point and bypass normal reparse point processing
    /// for the file. For more information, see the Remarks section.
    pub const FILE_OPEN_REPARSE_POINT: CreateOptions = CreateOptions(FILE_OPEN_REPARSE_POINT);

    /// Delete the file when the last handle to it is passed to NtClose. If this
    /// flag is set, the DELETE flag must be set in the DesiredAccess parameter.
    pub const FILE_DELETE_ON_CLOSE: CreateOptions = CreateOptions(FILE_DELETE_ON_CLOSE);

    /// The file name that is specified by the ObjectAttributes parameter includes
    /// the 8-byte file reference number for the file. This number is assigned by
    /// and specific to the particular file system. If the file is a reparse point,
    /// the file name will also include the name of a device. Note that the FAT file
    /// system does not support this flag. This flag is not used by device and
    /// intermediate drivers.
    pub const FILE_OPEN_BY_FILE_ID: CreateOptions = CreateOptions(FILE_OPEN_BY_FILE_ID);

    /// The file is being opened for backup intent. Therefore, the system should
    /// check for certain access rights and grant the caller the appropriate access
    /// to the file before checking the DesiredAccess parameter against the file's
    /// security descriptor. This flag not used by device and intermediate drivers.
    pub const FILE_OPEN_FOR_BACKUP_INTENT: CreateOptions =
        CreateOptions(FILE_OPEN_FOR_BACKUP_INTENT);

    /// This flag allows an application to request a filter opportunistic lock
    /// (oplock) to prevent other applications from getting share violations. If
    /// there are already open handles, the create request will fail with
    /// STATUS_OPLOCK_NOT_GRANTED. For more information, see the Remarks section.
    pub const FILE_RESERVE_OPFILTER: CreateOptions = CreateOptions(FILE_RESERVE_OPFILTER);

    /// The file is being opened and an opportunistic lock (oplock) on the file is
    /// being requested as a single atomic operation. The file system checks for
    /// oplocks before it performs the create operation and will fail the create
    /// with a return code of STATUS_CANNOT_BREAK_OPLOCK if the result would be to
    /// break an existing oplock. For more information, see the Remarks
    /// section.Windows Server 2008, Windows Vista, Windows Server 2003 and Windows
    /// XP:  This flag is not supported.
    ///
    /// This flag is supported on the following file systems: NTFS, FAT, and exFAT.
    pub const FILE_OPEN_REQUIRING_OPLOCK: CreateOptions = CreateOptions(FILE_OPEN_REQUIRING_OPLOCK);

    /// Complete this operation immediately with an alternate success code of
    /// STATUS_OPLOCK_BREAK_IN_PROGRESS if the target file is oplocked, rather than
    /// blocking the caller's thread. If the file is oplocked, another caller
    /// already has access to the file. This flag is not used by device and
    /// intermediate drivers.
    pub const FILE_COMPLETE_IF_OPLOCKED: CreateOptions = CreateOptions(FILE_COMPLETE_IF_OPLOCKED);

    pub const fn is(self, rhs: Self) -> bool {
        self.0 & rhs.0 != 0
    }
}

impl BitOr for CreateOptions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CreateOptions {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileAccessRights(pub FILE_ACCESS_RIGHTS);

impl_debug_flags!(FileAccessRights);

// Documentation taken from:
// - https://learn.microsoft.com/en-us/windows/win32/fileio/file-access-rights-constants
// - https://learn.microsoft.com/en-us/windows/win32/secauthz/standard-access-rights
// - https://learn.microsoft.com/en-us/windows/win32/fileio/file-security-and-access-rights
impl FileAccessRights {
    /// For a file object, the right to read the corresponding file data. For a
    /// directory object, the right to read the corresponding directory data.
    pub const FILE_READ_DATA: Self = Self(FILE_READ_DATA);

    /// The right to read extended file attributes.
    pub const FILE_READ_EA: Self = Self(FILE_READ_EA);

    /// For a directory, the right to list the contents of the directory.
    pub const FILE_LIST_DIRECTORY: Self = Self(FILE_LIST_DIRECTORY);

    /// For a file object, the right to write data to the file. For a directory
    /// object, the right to create a file in the directory (FILE_ADD_FILE).
    pub const FILE_WRITE_DATA: Self = Self(FILE_WRITE_DATA);

    /// For a directory, the right to create a file in the directory.
    pub const FILE_ADD_FILE: Self = Self(FILE_ADD_FILE);

    /// For a file object, the right to append data to the file. (For local files,
    /// write operations will not overwrite existing data if this flag is specified
    /// without FILE_WRITE_DATA.) For a directory object, the right to create a
    /// subdirectory (FILE_ADD_SUBDIRECTORY).
    pub const FILE_APPEND_DATA: Self = Self(FILE_APPEND_DATA);

    /// For a directory, the right to create a subdirectory.
    pub const FILE_ADD_SUBDIRECTORY: Self = Self(FILE_ADD_SUBDIRECTORY);

    /// For a named pipe, the right to create a pipe.
    pub const FILE_CREATE_PIPE_INSTANCE: Self = Self(FILE_CREATE_PIPE_INSTANCE);

    /// The right to write extended file attributes.
    pub const FILE_WRITE_EA: Self = Self(FILE_WRITE_EA);

    /// For a native code file, the right to execute the file. This access right
    /// given to scripts may cause the script to be executable, depending on the
    /// script interpreter.
    pub const FILE_EXECUTE: Self = Self(FILE_EXECUTE);

    /// For a directory, the right to traverse the directory. By default, users are
    /// assigned the BYPASS_TRAVERSE_CHECKING privilege, which ignores the
    /// FILE_TRAVERSE access right. See the remarks in File Security and Access
    /// Rights for more information.
    pub const FILE_TRAVERSE: Self = Self(FILE_TRAVERSE);

    /// For a directory, the right to delete a directory and all the files it
    /// contains, including read-only files.
    pub const FILE_DELETE_CHILD: Self = Self(FILE_DELETE_CHILD);

    /// The right to read file attributes.
    pub const FILE_READ_ATTRIBUTES: Self = Self(FILE_READ_ATTRIBUTES);

    /// The right to write file attributes.
    pub const FILE_WRITE_ATTRIBUTES: Self = Self(FILE_WRITE_ATTRIBUTES);

    /// The right to delete the object.
    pub const DELETE: Self = Self(DELETE);

    /// The right to read the information in the object's security descriptor, not
    /// including the information in the system access control list (SACL).
    pub const READ_CONTROL: Self = Self(READ_CONTROL);

    /// The right to modify the discretionary access control list (DACL) in the
    /// object's security descriptor.
    pub const WRITE_DAC: Self = Self(WRITE_DAC);

    /// The right to change the owner in the object's security descriptor.
    pub const WRITE_OWNER: Self = Self(WRITE_OWNER);

    /// The right to use the object for synchronization. This enables a thread to wait
    /// until the object is in the signaled state. Some object types do not support
    /// this access right.
    pub const SYNCHRONIZE: Self = Self(SYNCHRONIZE);

    /// Combines DELETE, READ_CONTROL, WRITE_DAC, and WRITE_OWNER access.
    pub const STANDARD_RIGHTS_REQUIRED: Self = Self(STANDARD_RIGHTS_REQUIRED);

    /// Currently defined to equal READ_CONTROL.
    pub const STANDARD_RIGHTS_READ: Self = Self(STANDARD_RIGHTS_READ);

    /// Currently defined to equal READ_CONTROL.
    pub const STANDARD_RIGHTS_WRITE: Self = Self(STANDARD_RIGHTS_WRITE);

    /// Same as STANDARD_RIGHTS_READ.
    pub const STANDARD_RIGHTS_EXECUTE: Self = Self(STANDARD_RIGHTS_EXECUTE);

    /// Combines DELETE, READ_CONTROL, WRITE_DAC, WRITE_OWNER, and SYNCHRONIZE access.
    pub const STANDARD_RIGHTS_ALL: Self = Self(STANDARD_RIGHTS_ALL);

    pub const SPECIFIC_RIGHTS_ALL: Self = Self(SPECIFIC_RIGHTS_ALL);

    /// All possible access rights for a file.
    pub const FILE_ALL_ACCESS: Self = Self(FILE_ALL_ACCESS);

    /// FILE_READ_ATTRIBUTES | FILE_READ_DATA | FILE_READ_EA | STANDARD_RIGHTS_READ | SYNCHRONIZE
    pub const FILE_GENERIC_READ: Self = Self(FILE_GENERIC_READ);

    /// FILE_APPEND_DATA | FILE_WRITE_ATTRIBUTES | FILE_WRITE_DATA | FILE_WRITE_EA | STANDARD_RIGHTS_WRITE | SYNCHRONIZE
    pub const FILE_GENERIC_WRITE: Self = Self(FILE_GENERIC_WRITE);

    /// FILE_EXECUTE | FILE_READ_ATTRIBUTES | STANDARD_RIGHTS_EXECUTE | SYNCHRONIZE
    pub const FILE_GENERIC_EXECUTE: Self = Self(FILE_GENERIC_EXECUTE);

    pub const fn is(self, rhs: Self) -> bool {
        self.0 & rhs.0 == rhs.0
    }
}

impl BitOr for FileAccessRights {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FileAccessRights {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CleanupFlags(pub i32);

impl_debug_flags!(CleanupFlags);

impl CleanupFlags {
    pub const DELETE: Self = Self(FspCleanupDelete);

    pub const SET_ALLOCATION_SIZE: Self = Self(FspCleanupSetAllocationSize);

    pub const SET_ARCHIVE_BIT: Self = Self(FspCleanupSetArchiveBit);

    pub const SET_LAST_ACCESS_TIME: Self = Self(FspCleanupSetLastAccessTime);

    pub const SET_LAST_WRITE_TIME: Self = Self(FspCleanupSetLastWriteTime);

    pub const SET_CHANGE_TIME: Self = Self(FspCleanupSetChangeTime);

    pub const fn is(self, rhs: Self) -> bool {
        self.0 & rhs.0 != 0
    }
}

impl BitOr for CleanupFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for CleanupFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileShareMode(pub FILE_SHARE_MODE);

impl_debug_flags!(FileShareMode);

// Documentation taken from https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilea
impl FileShareMode {
    /// Prevents other processes from opening a file or device if they request delete,
    /// read, or write access.
    pub const NONE: Self = Self(FILE_SHARE_NONE);

    /// Enables subsequent open operations on a file or device to request delete access.
    ///
    /// Otherwise, other processes cannot open the file or device if they request delete access.
    ///
    /// If this flag is not specified, but the file or device has been opened for delete access, the function fails.
    ///
    /// **Note**: Delete access allows both delete and rename operations.
    pub const DELETE: Self = Self(FILE_SHARE_DELETE);

    /// Enables subsequent open operations on a file or device to request read access.
    ///
    /// Otherwise, other processes cannot open the file or device if they request read access.
    ///
    /// If this flag is not specified, but the file or device has been opened for read access, the function fails.
    pub const READ: Self = Self(FILE_SHARE_READ);

    /// Enables subsequent open operations on a file or device to request write access.
    ///
    /// Otherwise, other processes cannot open the file or device if they request write access.
    ///
    /// If this flag is not specified, but the file or device has been opened for write access or has a file mapping with write access, the function fails.
    pub const WRITE: Self = Self(FILE_SHARE_WRITE);

    pub const fn is(self, rhs: Self) -> bool {
        self.0 & rhs.0 == rhs.0
    }
}

impl BitOr for FileShareMode {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FileShareMode {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0
    }
}

#[repr(u32)]
pub enum FileCreationDisposition {
    CreateNew = CREATE_NEW,
    CreateAlways = CREATE_ALWAYS,
    OpenExisting = OPEN_EXISTING,
    OpenAlways = OPEN_ALWAYS,
    TruncateExisting = TRUNCATE_EXISTING,
}
