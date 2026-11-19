//! This module is mostly about implementing the `FileSystemInterface` trait.
//!
//! The thing to consider here is we expose the `FileSystemInterface` trait as a high level
//! construct in order to build a structure of pointer (the `FSP_FILE_SYSTEM_INTERFACE` WinFSP
//! actually wants).
//!
//! However, there is no 1-to-1 relationship between trait implementation and struct of pointers:
//! a struct of pointer can have `NULL` pointers which is not possible to express when implementing
//! a trait (I've tried some tricks with function pointer comparison, but it doesn't support
//! methods with `foo: impl Type` parameter !)
//!
//! Hence, why we ask the end user to both implement the methods he needs AND set corresponding
//! `xxx_DEFINED` boolean (this way all methods with the boolean not set will be set as `NULL`
//! in the struct of pointers).
//!
//! ## Bonus: Why do we provide a `unreachable!()` default implementation for each method in the trait  ?
//!
//! Providing no default implementation means the end user implementing this trait would
//! have to implement all the methods.
//!
//! However, most of the time, not all methods need to be implemented (see for instance
//! the methods related to reparse points).
//!
//! In this case what should be the implementation of such method ?
//!
//! The obvious answer is "just implement with a unreachable and you're good to go !".
//! However, this has multiple drawbacks:
//! - It is much more verbose
//! - It feels very weird to define a method, but with a unreachable, so this method is not
//!   really "defined" and hence the `xxx_DEFINED` boolean should not be set !
//! - It is very tempting to implement those methods by returning a `NTSTATUS`
//!   `STATUS_NOT_IMPLEMENTED`... which cause very hard to track bugs ! (This used to be
//!   how winfsp_wrs worked, guessed how much time I spent pinpointing the issue ^^)
//!
//! So the alternative is set those default implementations in the trait, so this way the
//! end user only have to define the methods (and the corresponding `xxx_DEFINED`) he uses.

use std::sync::Arc;
use widestring::{U16CStr, U16CString};
use windows_sys::Win32::Foundation::{STATUS_BUFFER_OVERFLOW, STATUS_REPARSE, STATUS_SUCCESS};
use winfsp_wrs_sys::{
    FspFileSystemAddDirInfo, FspFileSystemFindReparsePoint, FspFileSystemResolveReparsePoints,
    FspFileSystemStopServiceIfNecessary, BOOLEAN, FSP_FILE_SYSTEM, FSP_FILE_SYSTEM_INTERFACE,
    FSP_FSCTL_DIR_INFO, FSP_FSCTL_FILE_INFO, FSP_FSCTL_VOLUME_INFO, NTSTATUS,
    PFILE_FULL_EA_INFORMATION, PIO_STATUS_BLOCK, PSECURITY_DESCRIPTOR, PSIZE_T, PUINT32, PULONG,
    PVOID, PWSTR, SECURITY_INFORMATION, SIZE_T, UINT32, UINT64, ULONG,
};

use crate::{
    CleanupFlags, CreateFileInfo, CreateOptions, DirInfo, FileAccessRights, FileAttributes,
    FileContextMode, FileInfo, PSecurityDescriptor, SecurityDescriptor, VolumeInfo, WriteMode,
};

/// Implement only if necessary at your own risk
pub trait FileContextKind {
    const MODE: FileContextMode;
    /// # Safety
    ///
    /// Write the data into winfsp's `PVOID *PFileContext`
    /// This is called in winfsp-wrs after calling FileSystemContext::open
    unsafe fn write(self, out: *mut PVOID);
    /// # Safety
    ///
    /// Retrieve the data from winfsp's `FileContext`
    /// This is called in winfsp-wrs before calling FileSystemContext::read/write etc.
    unsafe fn access(raw: PVOID) -> Self;
    /// # Safety
    ///
    /// Retrieve the data from winfsp's `FileContext`
    /// This is called in winfsp-wrs before calling FileSystemContext::close
    unsafe fn access_for_close(raw: PVOID) -> Self;
}

impl<T> FileContextKind for Arc<T> {
    const MODE: FileContextMode = FileContextMode::Descriptor;

    unsafe fn write(self, out: *mut PVOID) {
        out.write(Arc::into_raw(self).cast_mut().cast())
    }

    // the refcount must be incremented to keep the arc alive after being consumed
    // by drop
    unsafe fn access(raw: PVOID) -> Self {
        Arc::increment_strong_count(raw as *const T);
        Self::access_for_close(raw)
    }

    // the refcount should not be incremented so that when all operations are
    // complete the arc can be freed
    unsafe fn access_for_close(raw: PVOID) -> Self {
        Arc::from_raw(raw as *const T)
    }
}

impl FileContextKind for usize {
    const MODE: FileContextMode = FileContextMode::Node;

    // basic write
    unsafe fn write(self, out: *mut PVOID) {
        out.write(self as *mut _)
    }

    // basic access
    unsafe fn access(raw: PVOID) -> Self {
        raw as usize
    }

    // basic access
    unsafe fn access_for_close(raw: PVOID) -> Self {
        raw as usize
    }
}

/// High level interface over `FSP_FILE_SYSTEM_INTERFACE`.
///
/// This trait requires to overwrite all WinFSP callbacks you need and it corresponding
/// `xxx_DEFINED` associated const boolean.
///
/// This is needed to properly build the `FSP_FILE_SYSTEM_INTERFACE` struct, as
/// a callback pointer set to `NULL` (i.e. if `xxx_DEFINED=false`) leads to a different
/// behavior that a callback pointer containing a mock implementation (e.g.
/// returning `Err(STATUS_NOT_IMPLEMENTED)`).
///
/// So the way to work with this trait is to overwrite the method and `xxx_DEFINED` for
/// each function pointer you will need in `FSP_FILE_SYSTEM_INTERFACE`:
///
/// ```rust
/// struct MyFS;
/// impl FileSystemInterface for MyFS {
///     type FileContext: usize;
///     // `CREATE_DEFINED` not overwritten, hence `FSP_FILE_SYSTEM_INTERFACE.Create == NULL`
///     const CREATE_EX_DEFINED: bool = true;  // i.e. `FSP_FILE_SYSTEM_INTERFACE.CreateEx != NULL`
///     fn create_ex(
///         &self,
///         file_name: &U16CStr,
///         create_file_info: CreateFileInfo,
///         security_descriptor: SecurityDescriptor,
///         buffer: &[u8],
///         extra_buffer_is_reparse_point: bool,
///     ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
///         ...
///     }
/// }
/// ```
///
/// *Notes*:
/// - Associated method and `xxx_DEFINED` const must be overwritten together, as the
///   method is simply ignored if `xxx_DEFINED` is not set, and setting `xxx_DEFINED`
///   without overwritting the method means the function pointer relies on the method
///   default implementation that panics whenever used (ah !).
/// - If your are curious about the reason for using a trait here instead of a struct (or
///   associated const fields with `Option<fn()>` type in the trait instead of methods), it
///   all boils down to the fact some methods have an `impl Fn` function pointer as argument,
///   which is only possible in trait method.
pub trait FileSystemInterface {
    type FileContext: FileContextKind;

    const GET_VOLUME_INFO_DEFINED: bool = false;
    const SET_VOLUME_LABEL_DEFINED: bool = false;
    const GET_SECURITY_BY_NAME_DEFINED: bool = false;
    const CREATE_DEFINED: bool = false;
    const CREATE_EX_DEFINED: bool = false;
    const OPEN_DEFINED: bool = false;
    const OVERWRITE_DEFINED: bool = false;
    const OVERWRITE_EX_DEFINED: bool = false;
    const CLEANUP_DEFINED: bool = false;
    const CLOSE_DEFINED: bool = false;
    const READ_DEFINED: bool = false;
    const WRITE_DEFINED: bool = false;
    const FLUSH_DEFINED: bool = false;
    const GET_FILE_INFO_DEFINED: bool = false;
    const SET_BASIC_INFO_DEFINED: bool = false;
    const SET_FILE_SIZE_DEFINED: bool = false;
    const CAN_DELETE_DEFINED: bool = false;
    const RENAME_DEFINED: bool = false;
    const GET_SECURITY_DEFINED: bool = false;
    const SET_SECURITY_DEFINED: bool = false;
    const READ_DIRECTORY_DEFINED: bool = false;
    const GET_REPARSE_POINT_DEFINED: bool = false;
    const SET_REPARSE_POINT_DEFINED: bool = false;
    const DELETE_REPARSE_POINT_DEFINED: bool = false;
    const GET_STREAM_INFO_DEFINED: bool = false;
    const GET_DIR_INFO_BY_NAME_DEFINED: bool = false;
    const CONTROL_DEFINED: bool = false;
    const SET_DELETE_DEFINED: bool = false;
    const GET_EA_DEFINED: bool = false;
    const SET_EA_DEFINED: bool = false;
    const DISPATCHER_STOPPED_DEFINED: bool = false;
    const RESOLVE_REPARSE_POINTS_DEFINED: bool = false;

    /// Get volume information.
    fn get_volume_info(&self) -> Result<VolumeInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set volume label.
    fn set_volume_label(&self, _volume_label: &U16CStr) -> Result<VolumeInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get file or directory attributes and security descriptor given a file name.
    ///
    /// [out]:
    /// - file_attributes
    /// - security descriptor
    /// - reparse (false if `reparse_point` is not supported)
    ///
    /// [help]:
    /// - find_reparse_point (optional, can be ignored): Helper to find reparse
    ///   points (`get_reparse_point_by_name` should be implemented).
    ///   If reparse point is found, return the `FileAttributes` and `reparse` should be
    ///   set to `true`.
    fn get_security_by_name(
        &self,
        _file_name: &U16CStr,
        _find_reparse_point: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Create new file or directory.
    ///
    /// Note: `FileSystemContext::create_ex` takes precedence over `FileSystemContext::create`
    fn create(
        &self,
        _file_name: &U16CStr,
        _create_file_info: CreateFileInfo,
        _security_descriptor: SecurityDescriptor,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Create new file or directory.
    ///
    /// This function works like `create`, except that it also accepts an extra buffer
    /// that may contain extended attributes or a reparse point.
    ///
    /// Note: `FileSystemContext::create_ex` takes precedence over `FileSystemContext::create`
    fn create_ex(
        &self,
        _file_name: &U16CStr,
        _create_file_info: CreateFileInfo,
        _security_descriptor: SecurityDescriptor,
        _buffer: &[u8],
        _extra_buffer_is_reparse_point: bool,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Open a file or directory.
    fn open(
        &self,
        _file_name: &U16CStr,
        _create_options: CreateOptions,
        _granted_access: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Overwrite a file.
    ///
    /// Note: `FileSystemContext::overwrite_ex` takes precedence over `FileSystemContext::overwrite`
    fn overwrite(
        &self,
        _file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _replace_file_attributes: bool,
        _allocation_size: u64,
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Overwrite a file.
    ///
    /// This function works like `overwrite`, except that it also accepts EA (extended attributes).
    ///
    /// Note: `FileSystemContext::overwrite_ex` takes precedence over `FileSystemContext::overwrite`
    fn overwrite_ex(
        &self,
        _file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _buffer: &[u8],
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Cleanup a file.
    fn cleanup(
        &self,
        _file_context: Self::FileContext,
        _file_name: Option<&U16CStr>,
        _flags: CleanupFlags,
    ) {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Close a file.
    fn close(&self, _file_context: Self::FileContext) {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Read a file.
    fn read(
        &self,
        _file_context: Self::FileContext,
        _buffer: &mut [u8],
        _offset: u64,
    ) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Write a file.
    fn write(
        &self,
        _file_context: Self::FileContext,
        _buffer: &[u8],
        _mode: WriteMode,
    ) -> Result<(usize, FileInfo), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Flush a file or volume.
    fn flush(&self, _file_context: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get file or directory information.
    fn get_file_info(&self, _file_context: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set file or directory basic information.
    fn set_basic_info(
        &self,
        _file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _change_time: u64,
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set file/allocation size.
    fn set_file_size(
        &self,
        _file_context: Self::FileContext,
        _new_size: u64,
        _set_allocation_size: bool,
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Determine whether a file or directory can be deleted.
    ///
    /// Note: `FileSystemContext::set_delete` takes precedence over `FileSystemContext::can_delete`
    fn can_delete(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Renames a file or directory.
    fn rename(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
        _new_file_name: &U16CStr,
        _replace_if_exists: bool,
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get file or directory security descriptor.
    fn get_security(
        &self,
        _file_context: Self::FileContext,
    ) -> Result<PSecurityDescriptor, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set file or directory security descriptor.
    fn set_security(
        &self,
        _file_context: Self::FileContext,
        _security_information: u32,
        _modification_descriptor: PSecurityDescriptor,
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Read a directory.
    ///
    /// `add_dir_info` returns `false` if there is no more space left to add elements.
    fn read_directory(
        &self,
        _file_context: Self::FileContext,
        _marker: Option<&U16CStr>,
        _add_dir_info: impl FnMut(DirInfo) -> bool,
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get reparse point.
    fn get_reparse_point(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
        _buffer: &mut [u8],
    ) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set reparse point.
    fn set_reparse_point(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
        _buffer: &mut [u8],
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Delete reparse point.
    fn delete_reparse_point(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
        _buffer: &mut [u8],
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get named streams information.
    fn get_stream_info(
        &self,
        _file_context: Self::FileContext,
        _buffer: &mut [u8],
    ) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get directory information for a single file or directory within a parent
    /// directory.
    fn get_dir_info_by_name(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Process control code.
    fn control(
        &self,
        _file_context: Self::FileContext,
        _control_code: u32,
        _input_buffer: &[u8],
        _output_buffer: &mut [u8],
    ) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set the file delete flag.
    ///
    /// Note: `FileSystemContext::set_delete` takes precedence over `FileSystemContext::can_delete`
    fn set_delete(
        &self,
        _file_context: Self::FileContext,
        _file_name: &U16CStr,
        _delete_file: bool,
    ) -> Result<(), NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Get extended attributes.
    fn get_ea(&self, _file_context: Self::FileContext, _buffer: &[u8]) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// Set extended attributes.
    fn set_ea(
        &self,
        _file_context: Self::FileContext,
        _buffer: &[u8],
    ) -> Result<FileInfo, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }

    fn dispatcher_stopped(&self, _normally: bool) {
        unreachable!("To be used, trait method must be overwritten !");
    }

    /// quicKFS change: the stored spelling of the full path just opened or
    /// created, for WinFsp's file name normalization. A case-insensitive file
    /// system should report it; without it WinFsp upper-cases the name it
    /// keeps, which among other things makes a rename that only changes a
    /// name to upper case a silent no-op. `None` leaves WinFsp's default.
    fn normalized_name(&self, _file_context: &Self::FileContext) -> Option<U16CString> {
        None
    }

    /// Get reparse point given a file name.
    ///
    /// This method is used as a callback parameter to `FspFileSystemFindReparsePoint` &
    /// `FspFileSystemResolveReparsePoints` helpers to respectively implement
    /// `FSP_FILE_SYSTEM_INTERFACE`'s `GetSecurityByName` & `ResolveReparsePoints`.
    fn get_reparse_point_by_name(
        &self,
        _file_name: &U16CStr,
        _is_directory: bool,
        _buffer: Option<&mut [u8]>,
    ) -> Result<usize, NTSTATUS> {
        unreachable!("To be used, trait method must be overwritten !");
    }
}

/// `TrampolineInterface` fills the gap between the high level `FileSystemInterface`
/// and the `FSP_FILE_SYSTEM_INTERFACE` C struct that WinFSP expects from us.
pub(crate) struct TrampolineInterface;

impl TrampolineInterface {
    /// Get volume information.
    /// - FileSystem - The file system on which this request is posted.
    /// - VolumeInfo - [out] Pointer to a structure that will receive the volume
    ///   information on successful return from this call.
    unsafe extern "C" fn get_volume_info_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        volume_info: *mut FSP_FSCTL_VOLUME_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();

        match C::get_volume_info(fs) {
            Ok(vi) => {
                *volume_info = vi.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set volume label.
    /// - FileSystem - The file system on which this request is posted.
    /// - VolumeLabel - The new label for the volume.
    /// - VolumeInfo - [out] Pointer to a structure that will receive the volume
    ///   information on successful return from this call.
    unsafe extern "C" fn set_volume_label_w_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        volume_label: PWSTR,
        volume_info: *mut FSP_FSCTL_VOLUME_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();

        match C::set_volume_label(fs, U16CStr::from_ptr_str(volume_label)) {
            Ok(vi) => {
                *volume_info = vi.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Get file or directory attributes and security descriptor given a file name.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileName - The name of the file or directory to get the attributes and
    ///   security descriptor for.
    /// - PFileAttributes - Pointer to a memory location that will receive the file
    ///   attributes on successful return from this call. May be NULL.
    ///
    /// If this call returns STATUS_REPARSE, the file system MAY place here the
    /// index of the first reparse point within FileName. The file system MAY also
    /// leave this at its default value of 0.
    ///
    /// - SecurityDescriptor - Pointer to a buffer that will receive the file
    ///   security descriptor on successful return from this call. May be NULL.
    /// - PSecurityDescriptorSize - [in,out] Pointer to the security descriptor
    ///   buffer size. On input it contains the size of the security descriptor
    ///   buffer. On output it will contain the actual size of the security
    ///   descriptor copied into the security descriptor buffer. May be NULL.
    ///
    /// Remarks: STATUS_REPARSE should be returned by file systems that support
    /// reparse points when they encounter a FileName that contains reparse points
    /// anywhere but the final path component.
    unsafe extern "C" fn get_security_by_name_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_name: PWSTR,
        p_file_attributes: PUINT32,
        security_descriptor: PSECURITY_DESCRIPTOR,
        p_security_descriptor_size: *mut SIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();

        let find_reparse_point = || -> Option<FileAttributes> {
            let mut reparse_index = 0;
            unsafe {
                if FspFileSystemFindReparsePoint(
                    file_system,
                    Some(Self::get_reparse_point_by_name_ext::<C>),
                    std::ptr::null_mut(),
                    file_name,
                    &mut reparse_index,
                ) != 0
                {
                    Some(FileAttributes(reparse_index))
                } else {
                    None
                }
            }
        };

        let file_name = U16CStr::from_ptr_str(file_name);

        match C::get_security_by_name(fs, file_name, find_reparse_point) {
            Ok((fa, sd, reparse)) => {
                if !p_file_attributes.is_null() {
                    p_file_attributes.write(fa.0)
                }

                if !p_security_descriptor_size.is_null() {
                    if sd.len() as SIZE_T > p_security_descriptor_size.read() {
                        // In case of overflow error, winfsp will retry with a new
                        // allocation based on `p_security_descriptor_size`. Hence we
                        // must update this value to the required size.
                        p_security_descriptor_size.write(sd.len() as SIZE_T);
                        return STATUS_BUFFER_OVERFLOW;
                    }

                    p_security_descriptor_size.write(sd.len() as SIZE_T);

                    if !security_descriptor.is_null() {
                        std::ptr::copy(sd.inner(), security_descriptor, sd.len());
                    }
                }

                if reparse {
                    STATUS_REPARSE
                } else {
                    STATUS_SUCCESS
                }
            }
            Err(e) => e,
        }
    }

    /// Open a file or directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileName - The name of the file or directory to be opened.
    /// - CreateOptions - Create options for this request. This parameter has the
    ///   same meaning as the CreateOptions parameter of the NtCreateFile API. User
    ///   mode file systems typically do not need to do anything special with
    ///   respect to this parameter. Some file systems may also want to pay
    ///   attention to the FILE_NO_INTERMEDIATE_BUFFERING and FILE_WRITE_THROUGH
    ///   flags, although these are typically handled by the FSD component.
    /// - GrantedAccess - Determines the specific access rights that have been
    ///   granted for this request. Upon receiving this call all access checks have
    ///   been performed and the user mode file system need not perform any
    ///   additional checks. However this parameter may be useful to a user mode
    ///   file system; for example the WinFsp-FUSE layer uses this parameter to
    ///   determine which flags to use in its POSIX open() call.
    /// - PFileContext - [out] Pointer that will receive the file context on
    ///   successful return from this call.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    /// quicKFS change: report a normalized name through the
    /// `FSP_FSCTL_OPEN_FILE_INFO` that `Create` and `Open` really receive.
    unsafe fn write_normalized_name(file_info: *mut FSP_FSCTL_FILE_INFO, name: Option<U16CString>) {
        let Some(name) = name else {
            return;
        };
        let open = file_info.cast::<winfsp_wrs_sys::FSP_FSCTL_OPEN_FILE_INFO>();
        let buffer = (*open).NormalizedName;
        let bytes = name.as_slice().len() * std::mem::size_of::<u16>();
        if buffer.is_null() || bytes > usize::from((*open).NormalizedNameSize) {
            return;
        }
        std::ptr::copy_nonoverlapping(name.as_ptr(), buffer, name.as_slice().len());
        (*open).NormalizedNameSize = bytes as u16;
    }

    unsafe extern "C" fn open_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_name: PWSTR,
        create_options: UINT32,
        granted_access: UINT32,
        p_file_context: *mut PVOID,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let file_name = U16CStr::from_ptr_str(file_name);

        match C::open(
            fs,
            file_name,
            CreateOptions(create_options),
            FileAccessRights(granted_access),
        ) {
            Ok((fctx, finfo)) => {
                let normalized = C::normalized_name(fs, &fctx);
                C::FileContext::write(fctx, p_file_context);
                *file_info = finfo.0;
                Self::write_normalized_name(file_info, normalized);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Cleanup a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to cleanup.
    /// - FileName - The name of the file or directory to cleanup. Sent only when a
    ///   Delete is requested.
    /// - Flags - These flags determine whether the file was modified and whether
    ///   to delete the file.
    unsafe extern "C" fn cleanup_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        flags: ULONG,
    ) {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        let file_name = if file_name.is_null() {
            None
        } else {
            Some(U16CStr::from_ptr_str(file_name))
        };

        C::cleanup(fs, fctx, file_name, CleanupFlags(flags as i32))
    }

    /// Close a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to be closed.
    unsafe extern "C" fn close_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
    ) {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access_for_close(file_context);
        C::close(fs, fctx);
    }

    /// Read a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to be read.
    /// - Buffer - Pointer to a buffer that will receive the results of the read
    ///   operation.
    /// - Offset - Offset within the file to read from.
    /// - Length - Length of data to read.
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes read.
    unsafe extern "C" fn read_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        buffer: PVOID,
        offset: UINT64,
        length: ULONG,
        p_bytes_transferred: PULONG,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts_mut(buffer.cast(), length as usize)
        } else {
            &mut []
        };

        match C::read(fs, fctx, buffer, offset) {
            Ok(bytes_transferred) => {
                *p_bytes_transferred = bytes_transferred as ULONG;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Write a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to be written.
    /// - Buffer - Pointer to a buffer that contains the data to write.
    /// - Offset - Offset within the file to write to.
    /// - Length - Length of data to write.
    /// - WriteToEndOfFile - When TRUE the file system must write to the current
    ///   end of file. In this case the Offset parameter will contain the value -1.
    /// - ConstrainedIo - When TRUE the file system must not extend the file (i.e.
    ///   change the file size).
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes written.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn write_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        buffer: PVOID,
        offset: UINT64,
        length: ULONG,
        write_to_end_of_file: BOOLEAN,
        constrained_io: BOOLEAN,
        p_bytes_transferred: PULONG,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts(buffer.cast(), length as usize)
        } else {
            &[]
        };

        let mode = match (write_to_end_of_file != 0, constrained_io != 0) {
            (false, false) => WriteMode::Normal { offset },
            (false, true) => WriteMode::ConstrainedIO { offset },
            (true, false) => WriteMode::WriteToEOF,
            (true, true) => {
                *p_bytes_transferred = 0;
                return Self::get_file_info_ext::<C>(file_system, file_context, file_info);
            }
        };

        match C::write(fs, fctx, buffer, mode) {
            Ok((bytes_transfered, finfo)) => {
                *p_bytes_transferred = bytes_transfered as ULONG;
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Flush a file or volume.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to be flushed. When NULL the
    ///   whole volume is being flushed.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc. Used when flushing file (not
    ///   volume).
    unsafe extern "C" fn flush_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::flush(fs, fctx) {
            Ok(finfo) => {
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Get file or directory information.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to get
    ///   information for.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn get_file_info_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::get_file_info(fs, fctx) {
            Ok(ret) => {
                *file_info = ret.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set file or directory basic information.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to set
    ///   information for.
    /// - FileAttributes - File attributes to apply to the file or directory. If
    ///   the value INVALID_FILE_ATTRIBUTES is sent, the file attributes should not
    ///   be changed.
    /// - CreationTime - Creation time to apply to the file or directory. If the
    ///   value 0 is sent, the creation time should not be changed.
    /// - LastAccessTime - Last access time to apply to the file or directory. If
    ///   the value 0 is sent, the last access time should not be changed.
    /// - LastWriteTime - Last write time to apply to the file or directory. If the
    ///   value 0 is sent, the last write time should not be changed.
    /// - ChangeTime - Change time to apply to the file or directory. If the value
    ///   0 is sent, the change time should not be changed.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn set_basic_info_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_attributes: UINT32,
        creation_time: UINT64,
        last_access_time: UINT64,
        last_write_time: UINT64,
        change_time: UINT64,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::set_basic_info(
            fs,
            fctx,
            FileAttributes(file_attributes),
            creation_time,
            last_access_time,
            last_write_time,
            change_time,
        ) {
            Ok(finfo) => {
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set file/allocation size.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to set the file/allocation
    ///   size for.
    /// - NewSize - New file/allocation size to apply to the file.
    /// - SetAllocationSize - If TRUE, then the allocation size is being set. if
    ///   FALSE, then the file size is being set.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn set_file_size_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        new_size: UINT64,
        set_allocation_size: BOOLEAN,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::set_file_size(fs, fctx, new_size, set_allocation_size != 0) {
            Ok(finfo) => {
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Determine whether a file or directory can be deleted.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to cleanup.
    /// - FileName - The name of the file or directory to cleanup. Sent only when a
    ///   Delete is requested.
    /// - Flags - These flags determine whether the file was modified and whether
    ///   to delete the file.
    unsafe extern "C" fn can_delete_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);

        match C::can_delete(fs, fctx, file_name) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Renames a file or directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to be renamed.
    /// - FileName - The current name of the file or directory to rename.
    /// - NewFileName - The new name for the file or directory.
    /// - ReplaceIfExists - Whether to replace a file that already exists at
    ///   NewFileName.
    unsafe extern "C" fn rename_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        new_file_name: PWSTR,
        replace_if_exists: BOOLEAN,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);
        let new_file_name = U16CStr::from_ptr_str(new_file_name);

        match C::rename(fs, fctx, file_name, new_file_name, replace_if_exists != 0) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Get file or directory security descriptor.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to get the
    ///   security descriptor for.
    /// - SecurityDescriptor - Pointer to a buffer that will receive the file
    ///   security descriptor on successful return from this call. May be NULL.
    /// - PSecurityDescriptorSize - [in,out] Pointer to the security descriptor
    ///   buffer size. On input it contains the size of the security descriptor
    ///   buffer. On output it will contain the actual size of the security
    ///   descriptor copied into the security descriptor buffer. Cannot be NULL.
    unsafe extern "C" fn get_security_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        security_descriptor: PSECURITY_DESCRIPTOR,
        p_security_descriptor_size: *mut SIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::get_security(fs, fctx) {
            Ok(sd) => {
                if !p_security_descriptor_size.is_null() {
                    if sd.len() as SIZE_T > p_security_descriptor_size.read() {
                        return STATUS_BUFFER_OVERFLOW;
                    }
                    p_security_descriptor_size.write(sd.len() as SIZE_T);
                    if !security_descriptor.is_null() {
                        std::ptr::copy(sd.inner(), security_descriptor, sd.len())
                    }
                }

                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set file or directory security descriptor.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to set the
    ///   security descriptor for.
    /// - SecurityInformation - Describes what parts of the file or directory
    ///   security descriptor should be modified.
    /// - ModificationDescriptor - Describes the modifications to apply to the file
    ///   or directory security descriptor.
    unsafe extern "C" fn set_security_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        security_information: SECURITY_INFORMATION,
        modification_descriptor: PSECURITY_DESCRIPTOR,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        let modification_descriptor = PSecurityDescriptor::from_ptr(modification_descriptor);

        match C::set_security(fs, fctx, security_information, modification_descriptor) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Read a directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the directory to be read.
    /// - Pattern - The pattern to match against files in this directory. Can be
    ///   NULL. The file system can choose to ignore this parameter as the FSD will
    ///   always perform its own pattern matching on the returned results.
    /// - Marker - A file name that marks where in the directory to start reading.
    ///   Files with names that are greater than (not equal to) this marker (in the
    ///   directory order determined by the file system) should be returned. Can be
    ///   NULL.
    /// - Buffer - Pointer to a buffer that will receive the results of the read
    ///   operation.
    /// - Length - Length of data to read.
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes read.
    unsafe extern "C" fn read_directory_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        _pattern: PWSTR,
        marker: PWSTR,
        buffer: PVOID,
        length: ULONG,
        p_bytes_transferred: PULONG,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        let marker = if marker.is_null() {
            None
        } else {
            Some(U16CStr::from_ptr_str(marker))
        };

        let mut buffer_full = false;
        let add_dir_info = |mut dir_info: DirInfo| {
            let added = FspFileSystemAddDirInfo(
                (&mut dir_info as *mut DirInfo).cast(),
                buffer,
                length,
                p_bytes_transferred,
            ) != 0;
            if !added {
                buffer_full = true;
            }
            added
        };

        match C::read_directory(fs, fctx, marker, add_dir_info) {
            Ok(()) => {
                if !buffer_full {
                    // EOF marker
                    FspFileSystemAddDirInfo(
                        std::ptr::null_mut(),
                        buffer,
                        length,
                        p_bytes_transferred,
                    );
                }
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    unsafe extern "C" fn get_reparse_point_by_name_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        _context: PVOID,
        file_name: PWSTR,
        is_directory: BOOLEAN,
        buffer: PVOID,
        psize: PSIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let file_name = U16CStr::from_ptr_str_mut(file_name);
        let buffer = if !buffer.is_null() {
            Some(std::slice::from_raw_parts_mut(
                buffer.cast(),
                psize.read() as usize,
            ))
        } else {
            None
        };

        match C::get_reparse_point_by_name(fs, file_name, is_directory != 0, buffer) {
            Ok(bytes_transferred) => {
                // quicKFS change: `FspFileSystemFindReparsePoint` probes with
                // a null buffer *and* a null size pointer; upstream wrote the
                // size unconditionally, dereferencing null.
                if !psize.is_null() {
                    psize.write(bytes_transferred as SIZE_T);
                }
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Resolve reparse points.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileName - The name of the file or directory to have its reparse points
    ///   resolved.
    /// - ReparsePointIndex - The index of the first reparse point within FileName.
    /// - ResolveLastPathComponent - If FALSE, the last path component of FileName
    ///   should not be resolved, even if it is a reparse point that can be
    ///   resolved. If TRUE, all path components should be resolved if possible.
    /// - PIoStatus - Pointer to storage that will receive the status to return to
    ///   the FSD. When this function succeeds it must set PIoStatus->Status to
    ///   STATUS_REPARSE and PIoStatus->Information to either IO_REPARSE or the
    ///   reparse tag.
    /// - Buffer - Pointer to a buffer that will receive the resolved file name
    ///   (IO_REPARSE) or reparse data (reparse tag). If the function returns a
    ///   file name, it should not be NULL terminated.
    /// - PSize - [in,out] Pointer to the buffer size. On input it contains the
    ///   size of the buffer. On output it will contain the actual size of data
    ///   copied.
    unsafe extern "C" fn resolve_reparse_points_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_name: PWSTR,
        reparse_point_index: UINT32,
        resolve_last_path_component: BOOLEAN,
        p_io_status: PIO_STATUS_BLOCK,
        buffer: PVOID,
        p_size: PSIZE_T,
    ) -> NTSTATUS {
        FspFileSystemResolveReparsePoints(
            file_system,
            Some(Self::get_reparse_point_by_name_ext::<C>),
            std::ptr::null_mut(),
            file_name,
            reparse_point_index,
            resolve_last_path_component,
            p_io_status,
            buffer,
            p_size,
        )
    }

    /// Get reparse point.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the reparse point.
    /// - FileName - The file name of the reparse point.
    /// - Buffer - Pointer to a buffer that will receive the results of this
    ///   operation. If the function returns a symbolic link path, it should not be
    ///   NULL terminated.
    /// - PSize - [in,out] Pointer to the buffer size. On input it contains the
    ///   size of the buffer. On output it will contain the actual size of data
    ///   copied.
    unsafe extern "C" fn get_reparse_point_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        buffer: PVOID,
        p_size: PSIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts_mut(buffer.cast(), *p_size as usize)
        } else {
            &mut []
        };

        match C::get_reparse_point(fs, fctx, file_name, buffer) {
            Ok(byte_transferred) => {
                p_size.write(byte_transferred as SIZE_T);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set reparse point.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the reparse point.
    /// - FileName - The file name of the reparse point.
    /// - Buffer - Pointer to a buffer that contains the data for this operation.
    ///   If this buffer contains a symbolic link path, it should not be assumed to
    ///   be NULL terminated.
    /// - Size - Size of data to write.
    unsafe extern "C" fn set_reparse_point_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        buffer: PVOID,
        size: SIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts_mut(buffer.cast(), size as usize)
        } else {
            &mut []
        };

        match C::set_reparse_point(fs, fctx, file_name, buffer) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Delete reparse point.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the reparse point.
    /// - FileName - The file name of the reparse point.
    /// - Buffer - Pointer to a buffer that contains the data for this operation.
    /// - Size - Size of data to write.
    unsafe extern "C" fn delete_reparse_point_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        buffer: PVOID,
        size: SIZE_T,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts_mut(buffer.cast(), size as usize)
        } else {
            &mut []
        };

        match C::delete_reparse_point(fs, fctx, file_name, buffer) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Get named streams information.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to get stream
    ///   information for.
    /// - Buffer - Pointer to a buffer that will receive the stream information.
    /// - Length - Length of buffer.
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes stored.
    unsafe extern "C" fn get_stream_info_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        buffer: PVOID,
        length: ULONG,
        p_bytes_transferred: PULONG,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !buffer.is_null() {
            std::slice::from_raw_parts_mut(buffer.cast(), length as usize)
        } else {
            &mut []
        };

        match C::get_stream_info(fs, fctx, buffer) {
            Ok(bytes_transferred) => {
                p_bytes_transferred.write(bytes_transferred as ULONG);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Get directory information for a single file or directory within a parent
    /// directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the parent directory.
    /// - FileName - The name of the file or directory to get information for. This
    ///   name is relative to the parent directory and is a single path component.
    /// - DirInfo - [out] Pointer to a structure that will receive the directory
    ///   information on successful return from this call. This information
    ///   includes the file name, but also file attributes, file times, etc.
    unsafe extern "C" fn get_dir_info_by_name_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        dir_info: *mut FSP_FSCTL_DIR_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);

        match C::get_dir_info_by_name(fs, fctx, file_name) {
            Ok(finfo) => {
                (*dir_info).Size =
                    (std::mem::size_of::<FSP_FSCTL_DIR_INFO>() + file_name.len() * 2) as u16;
                (*dir_info).FileInfo = finfo.0;
                std::ptr::copy(
                    file_name.as_ptr(),
                    (*dir_info).FileNameBuf.as_mut_ptr(),
                    file_name.len(),
                );
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Process control code.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to be controled.
    /// - ControlCode - The control code for the operation. This code must have a
    ///   DeviceType with bit 0x8000 set and must have a TransferType of
    ///   METHOD_BUFFERED.
    /// - InputBuffer - Pointer to a buffer that contains the input data.
    /// - InputBufferLength - Input data length.
    /// - OutputBuffer - Pointer to a buffer that will receive the output data.
    /// - OutputBufferLength - Output data length.
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes transferred.
    unsafe extern "C" fn control_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        control_code: UINT32,
        input_buffer: PVOID,
        input_buffer_length: ULONG,
        output_buffer: PVOID,
        output_buffer_length: ULONG,
        p_bytes_transferred: PULONG,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let input = if !input_buffer.is_null() {
            std::slice::from_raw_parts(input_buffer.cast(), input_buffer_length as usize)
        } else {
            &[]
        };
        let output = if !output_buffer.is_null() {
            std::slice::from_raw_parts_mut(output_buffer.cast(), output_buffer_length as usize)
        } else {
            &mut []
        };

        match C::control(fs, fctx, control_code, input, output) {
            Ok(bytes_transferred) => {
                p_bytes_transferred.write(bytes_transferred as ULONG);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set the file delete flag.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file or directory to set the delete
    ///   flag for.
    /// - FileName - The name of the file or directory to set the delete flag for.
    /// - DeleteFile - If set to TRUE the FSD indicates that the file will be
    ///   deleted on Cleanup; otherwise it will not be deleted. It is legal to
    ///   receive multiple SetDelete calls for the same file with different
    ///   DeleteFile parameters.
    unsafe extern "C" fn set_delete_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_name: PWSTR,
        delete_file_w: BOOLEAN,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let file_name = U16CStr::from_ptr_str(file_name);

        match C::set_delete(fs, fctx, file_name, delete_file_w != 0) {
            Ok(()) => STATUS_SUCCESS,
            Err(e) => e,
        }
    }

    /// Create new file or directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileName - The name of the file or directory to be created.
    /// - CreateOptions - Create options for this request. This parameter has the
    ///   same meaning as the CreateOptions parameter of the NtCreateFile API. User
    ///   mode file systems should typically only be concerned with the flag
    ///   FILE_DIRECTORY_FILE, which is an instruction to create a directory rather
    ///   than a file. Some file systems may also want to pay attention to the
    ///   FILE_NO_INTERMEDIATE_BUFFERING and FILE_WRITE_THROUGH flags, although
    ///   these are typically handled by the FSD component.
    /// - GrantedAccess - Determines the specific access rights that have been
    ///   granted for this request. Upon receiving this call all access checks have
    ///   been performed and the user mode file system need not perform any
    ///   additional checks. However this parameter may be useful to a user mode
    ///   file system; for example the WinFsp-FUSE layer uses this parameter to
    ///   determine which flags to use in its POSIX open() call.
    /// - FileAttributes - File attributes to apply to the newly created file or
    ///   directory.
    /// - SecurityDescriptor - Security descriptor to apply to the newly created
    ///   file or directory. This security descriptor will always be in
    ///   self-relative format. Its length can be retrieved using the Windows
    ///   GetSecurityDescriptorLength API. Will be NULL for named streams.
    /// - AllocationSize - Allocation size for the newly created file.
    /// - PFileContext - [out] Pointer that will receive the file context on
    ///   successful return from this call.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn create_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_name: PWSTR,
        create_options: UINT32,
        granted_access: UINT32,
        file_attributes: UINT32,
        security_descriptor: PSECURITY_DESCRIPTOR,
        allocation_size: UINT64,
        p_file_context: *mut PVOID,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let file_name = U16CStr::from_ptr_str(file_name);
        let sd = SecurityDescriptor::from_ptr(security_descriptor);

        match C::create(
            fs,
            file_name,
            CreateFileInfo {
                create_options: CreateOptions(create_options),
                granted_access: FileAccessRights(granted_access),
                file_attributes: FileAttributes(file_attributes),
                allocation_size,
            },
            sd,
        ) {
            Ok((fctx, finfo)) => {
                let normalized = C::normalized_name(fs, &fctx);
                C::FileContext::write(fctx, p_file_context);
                *file_info = finfo.0;
                Self::write_normalized_name(file_info, normalized);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Create new file or directory.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileName - The name of the file or directory to be created.
    /// - CreateOptions - Create options for this request. This parameter has the
    ///   same meaning as the CreateOptions parameter of the NtCreateFile API. User
    ///   mode file systems should typically only be concerned with the flag
    ///   FILE_DIRECTORY_FILE, which is an instruction to create a directory rather
    ///   than a file. Some file systems may also want to pay attention to the
    ///   FILE_NO_INTERMEDIATE_BUFFERING and FILE_WRITE_THROUGH flags, although
    ///   these are typically handled by the FSD component.
    /// - GrantedAccess - Determines the specific access rights that have been
    ///   granted for this request. Upon receiving this call all access checks have
    ///   been performed and the user mode file system need not perform any
    ///   additional checks. However this parameter may be useful to a user mode
    ///   file system; for example the WinFsp-FUSE layer uses this parameter to
    ///   determine which flags to use in its POSIX open() call.
    /// - FileAttributes - File attributes to apply to the newly created file or
    ///   directory.
    /// - SecurityDescriptor - Security descriptor to apply to the newly created
    ///   file or directory. This security descriptor will always be in
    ///   self-relative format. Its length can be retrieved using the Windows
    ///   GetSecurityDescriptorLength API. Will be NULL for named streams.
    /// - AllocationSize - Allocation size for the newly created file.
    /// - ExtraBuffer - Extended attributes or reparse point buffer.
    /// - ExtraLength - Extended attributes or reparse point buffer length.
    /// - ExtraBufferIsReparsePoint - FALSE: extra buffer is extended attributes;
    ///   TRUE: extra buffer is reparse point.
    /// - PFileContext - [out] Pointer that will receive the file context on
    ///   successful return from this call.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn create_ex_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_name: PWSTR,
        create_options: UINT32,
        granted_access: UINT32,
        file_attributes: UINT32,
        security_descriptor: PSECURITY_DESCRIPTOR,
        allocation_size: UINT64,
        extra_buffer: PVOID,
        extra_length: ULONG,
        extra_buffer_is_reparse_point: BOOLEAN,
        p_file_context: *mut PVOID,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let file_name = U16CStr::from_ptr_str(file_name);
        let sd = SecurityDescriptor::from_ptr(security_descriptor);
        let buffer = if !extra_buffer.is_null() {
            std::slice::from_raw_parts(extra_buffer.cast(), extra_length as usize)
        } else {
            &[]
        };

        match C::create_ex(
            fs,
            file_name,
            CreateFileInfo {
                create_options: CreateOptions(create_options),
                granted_access: FileAccessRights(granted_access),
                file_attributes: FileAttributes(file_attributes),
                allocation_size,
            },
            sd,
            buffer,
            extra_buffer_is_reparse_point != 0,
        ) {
            Ok((fctx, finfo)) => {
                let normalized = C::normalized_name(fs, &fctx);
                C::FileContext::write(fctx, p_file_context);
                *file_info = finfo.0;
                Self::write_normalized_name(file_info, normalized);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Overwrite a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to overwrite.
    /// - FileAttributes - File attributes to apply to the overwritten file.
    /// - ReplaceFileAttributes - When TRUE the existing file attributes should be
    ///   replaced with the new ones. When FALSE the existing file attributes
    ///   should be merged (or'ed) with the new ones.
    /// - AllocationSize - Allocation size for the overwritten file.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn overwrite_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_attributes: UINT32,
        replace_file_attributes: BOOLEAN,
        allocation_size: UINT64,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);

        match C::overwrite(
            fs,
            fctx,
            FileAttributes(file_attributes),
            replace_file_attributes != 0,
            allocation_size,
        ) {
            Ok(finfo) => {
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Overwrite a file.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to overwrite.
    /// - FileAttributes - File attributes to apply to the overwritten file.
    /// - ReplaceFileAttributes - When TRUE the existing file attributes should be
    ///   replaced with the new ones. When FALSE the existing file attributes
    ///   should be merged (or'ed) with the new ones.
    /// - AllocationSize - Allocation size for the overwritten file.
    /// - Ea - Extended attributes buffer.
    /// - EaLength - Extended attributes buffer length.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn overwrite_ex_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        file_attributes: UINT32,
        replace_file_attributes: BOOLEAN,
        allocation_size: UINT64,
        ea: PFILE_FULL_EA_INFORMATION,
        ea_length: ULONG,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !ea.is_null() {
            std::slice::from_raw_parts(ea.cast(), ea_length as usize)
        } else {
            &[]
        };

        match C::overwrite_ex(
            fs,
            fctx,
            FileAttributes(file_attributes),
            replace_file_attributes != 0,
            allocation_size,
            buffer,
        ) {
            Ok(finfo) => {
                *file_info = finfo.0;
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Get extended attributes.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to get extended attributes
    ///   for.
    /// - Ea - Extended attributes buffer.
    /// - EaLength - Extended attributes buffer length.
    /// - PBytesTransferred - [out] Pointer to a memory location that will receive
    ///   the actual number of bytes transferred.
    unsafe extern "C" fn get_ea_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        ea: PFILE_FULL_EA_INFORMATION,
        ea_length: ULONG,
        p_bytes_transferred: PULONG,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !ea.is_null() {
            std::slice::from_raw_parts(ea.cast(), ea_length as usize)
        } else {
            &[]
        };

        match C::get_ea(fs, fctx, buffer) {
            Ok(bytes_transfered) => {
                p_bytes_transferred.write(bytes_transfered as ULONG);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    /// Set extended attributes.
    /// - FileSystem - The file system on which this request is posted.
    /// - FileContext - The file context of the file to set extended attributes
    ///   for.
    /// - Ea - Extended attributes buffer.
    /// - EaLength - Extended attributes buffer length.
    /// - FileInfo - [out] Pointer to a structure that will receive the file
    ///   information on successful return from this call. This information
    ///   includes file attributes, file times, etc.
    unsafe extern "C" fn set_ea_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        file_context: PVOID,
        ea: PFILE_FULL_EA_INFORMATION,
        ea_length: ULONG,
        file_info: *mut FSP_FSCTL_FILE_INFO,
    ) -> NTSTATUS {
        let fs = &*(*file_system).UserContext.cast::<C>();
        let fctx = C::FileContext::access(file_context);
        let buffer = if !ea.is_null() {
            std::slice::from_raw_parts(ea.cast(), ea_length as usize)
        } else {
            &[]
        };

        match C::set_ea(fs, fctx, buffer) {
            Ok(info) => {
                file_info.write(info.0);
                STATUS_SUCCESS
            }
            Err(e) => e,
        }
    }

    unsafe extern "C" fn dispatcher_stopped_ext<C: FileSystemInterface>(
        file_system: *mut FSP_FILE_SYSTEM,
        normally: BOOLEAN,
    ) {
        let fs = &*(*file_system).UserContext.cast::<C>();

        C::dispatcher_stopped(fs, normally != 0);

        FspFileSystemStopServiceIfNecessary(file_system, normally)
    }

    pub(crate) fn interface<Ctx: FileSystemInterface>() -> FSP_FILE_SYSTEM_INTERFACE {
        macro_rules! set_fn_pointer_or_null {
            ($flag_name:ident, $fn_ext_name:ident) => {
                if Ctx::$flag_name {
                    Some(Self::$fn_ext_name::<Ctx>)
                } else {
                    None
                }
            };
        }

        FSP_FILE_SYSTEM_INTERFACE {
            GetVolumeInfo: set_fn_pointer_or_null!(GET_VOLUME_INFO_DEFINED, get_volume_info_ext),
            SetVolumeLabelW: set_fn_pointer_or_null!(
                SET_VOLUME_LABEL_DEFINED,
                set_volume_label_w_ext
            ),
            GetSecurityByName: set_fn_pointer_or_null!(
                GET_SECURITY_BY_NAME_DEFINED,
                get_security_by_name_ext
            ),
            Create: set_fn_pointer_or_null!(CREATE_DEFINED, create_ext),
            CreateEx: set_fn_pointer_or_null!(CREATE_EX_DEFINED, create_ex_ext),
            Open: set_fn_pointer_or_null!(OPEN_DEFINED, open_ext),
            Overwrite: set_fn_pointer_or_null!(OVERWRITE_DEFINED, overwrite_ext),
            OverwriteEx: set_fn_pointer_or_null!(OVERWRITE_EX_DEFINED, overwrite_ex_ext),
            Cleanup: set_fn_pointer_or_null!(CLEANUP_DEFINED, cleanup_ext),
            Close: set_fn_pointer_or_null!(CLOSE_DEFINED, close_ext),
            Read: set_fn_pointer_or_null!(READ_DEFINED, read_ext),
            Write: set_fn_pointer_or_null!(WRITE_DEFINED, write_ext),
            Flush: set_fn_pointer_or_null!(FLUSH_DEFINED, flush_ext),
            GetFileInfo: set_fn_pointer_or_null!(GET_FILE_INFO_DEFINED, get_file_info_ext),
            SetBasicInfo: set_fn_pointer_or_null!(SET_BASIC_INFO_DEFINED, set_basic_info_ext),
            SetFileSize: set_fn_pointer_or_null!(SET_FILE_SIZE_DEFINED, set_file_size_ext),
            CanDelete: set_fn_pointer_or_null!(CAN_DELETE_DEFINED, can_delete_ext),
            Rename: set_fn_pointer_or_null!(RENAME_DEFINED, rename_ext),
            GetSecurity: set_fn_pointer_or_null!(GET_SECURITY_DEFINED, get_security_ext),
            SetSecurity: set_fn_pointer_or_null!(SET_SECURITY_DEFINED, set_security_ext),
            ReadDirectory: set_fn_pointer_or_null!(READ_DIRECTORY_DEFINED, read_directory_ext),
            GetReparsePoint: set_fn_pointer_or_null!(
                GET_REPARSE_POINT_DEFINED,
                get_reparse_point_ext
            ),
            SetReparsePoint: set_fn_pointer_or_null!(
                SET_REPARSE_POINT_DEFINED,
                set_reparse_point_ext
            ),
            DeleteReparsePoint: set_fn_pointer_or_null!(
                DELETE_REPARSE_POINT_DEFINED,
                delete_reparse_point_ext
            ),
            GetStreamInfo: set_fn_pointer_or_null!(GET_STREAM_INFO_DEFINED, get_stream_info_ext),
            GetDirInfoByName: set_fn_pointer_or_null!(
                GET_DIR_INFO_BY_NAME_DEFINED,
                get_dir_info_by_name_ext
            ),
            Control: set_fn_pointer_or_null!(CONTROL_DEFINED, control_ext),
            SetDelete: set_fn_pointer_or_null!(SET_DELETE_DEFINED, set_delete_ext),
            GetEa: set_fn_pointer_or_null!(GET_EA_DEFINED, get_ea_ext),
            SetEa: set_fn_pointer_or_null!(SET_EA_DEFINED, set_ea_ext),
            DispatcherStopped: set_fn_pointer_or_null!(
                DISPATCHER_STOPPED_DEFINED,
                dispatcher_stopped_ext
            ),
            ResolveReparsePoints: set_fn_pointer_or_null!(
                RESOLVE_REPARSE_POINTS_DEFINED,
                resolve_reparse_points_ext
            ),

            ..Default::default() // Initializing `Obsolete0` & `Reserved` fields
        }
    }
}
