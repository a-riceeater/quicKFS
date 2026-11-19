mod callback;
mod file_system;
mod filetime;
mod flags;
mod info;
mod init;
mod security;

pub(crate) use callback::TrampolineInterface;

pub use callback::{FileContextKind, FileSystemInterface};
#[cfg(feature = "icon")]
pub use file_system::set_folder_icon;
pub use file_system::{
    pin_to_quick_access, unpin_to_quick_access, FileContextMode, FileSystem,
    OperationGuardStrategy, Params, VolumeParams,
};
pub use filetime::{filetime_from_utc, filetime_now};
pub use flags::{
    CleanupFlags, CreateOptions, FileAccessRights, FileAttributes, FileCreationDisposition,
    FileShareMode,
};
pub use info::{CreateFileInfo, DirInfo, FileInfo, VolumeInfo, VolumeLabelNameTooLong, WriteMode};
pub use init::{init, InitError};
pub use security::{PSecurityDescriptor, SecurityDescriptor};

// Reexport
pub use widestring::*;
pub use windows_sys::Win32::Foundation::*;
