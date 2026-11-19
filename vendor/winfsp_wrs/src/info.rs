use widestring::{U16CStr, U16Str};
use winfsp_wrs_sys::{FSP_FSCTL_DIR_INFO, FSP_FSCTL_FILE_INFO, FSP_FSCTL_VOLUME_INFO};

use crate::{CreateOptions, FileAccessRights, FileAttributes};

#[derive(Debug, Default, Clone, Copy)]
pub struct FileInfo(pub(crate) FSP_FSCTL_FILE_INFO);

impl FileInfo {
    pub const fn file_attributes(&self) -> FileAttributes {
        FileAttributes(self.0.FileAttributes)
    }

    pub const fn reparse_tag(&self) -> u32 {
        self.0.ReparseTag
    }

    pub const fn allocation_size(&self) -> u64 {
        self.0.AllocationSize
    }

    pub const fn file_size(&self) -> u64 {
        self.0.FileSize
    }

    pub const fn creation_time(&self) -> u64 {
        self.0.CreationTime
    }

    pub const fn last_access_time(&self) -> u64 {
        self.0.LastAccessTime
    }

    pub const fn last_write_time(&self) -> u64 {
        self.0.LastWriteTime
    }

    pub const fn change_time(&self) -> u64 {
        self.0.ChangeTime
    }

    pub const fn index_number(&self) -> u64 {
        self.0.IndexNumber
    }

    pub const fn hard_links(&self) -> u32 {
        self.0.HardLinks
    }

    pub const fn ea_size(&self) -> u32 {
        self.0.EaSize
    }

    pub fn set_file_attributes(&mut self, val: FileAttributes) -> &mut Self {
        self.0.FileAttributes = val.0;
        self
    }

    pub fn set_reparse_tag(&mut self, val: u32) -> &mut Self {
        self.0.ReparseTag = val;
        self
    }

    pub fn set_allocation_size(&mut self, val: u64) -> &mut Self {
        self.0.AllocationSize = val;
        self
    }

    pub fn set_file_size(&mut self, val: u64) -> &mut Self {
        self.0.FileSize = val;
        self
    }

    pub fn set_creation_time(&mut self, val: u64) -> &mut Self {
        self.0.CreationTime = val;
        self
    }

    pub fn set_last_access_time(&mut self, val: u64) -> &mut Self {
        self.0.LastAccessTime = val;
        self
    }

    pub fn set_last_write_time(&mut self, val: u64) -> &mut Self {
        self.0.LastWriteTime = val;
        self
    }

    pub fn set_change_time(&mut self, val: u64) -> &mut Self {
        self.0.ChangeTime = val;
        self
    }

    pub fn set_time(&mut self, val: u64) -> &mut Self {
        self.0.CreationTime = val;
        self.0.LastAccessTime = val;
        self.0.LastWriteTime = val;
        self.0.ChangeTime = val;
        self
    }

    pub fn set_index_number(&mut self, val: u64) -> &mut Self {
        self.0.IndexNumber = val;
        self
    }

    pub fn set_hard_links(&mut self, val: u32) -> &mut Self {
        self.0.HardLinks = val;
        self
    }

    pub fn set_ea_size(&mut self, val: u32) -> &mut Self {
        self.0.EaSize = val;
        self
    }
}

#[derive(Debug, Default, Clone)]
pub struct VolumeInfo(pub(crate) FSP_FSCTL_VOLUME_INFO);

#[derive(Debug)]
pub struct VolumeLabelNameTooLong;

impl VolumeInfo {
    // Max len correspond to the entire `FSP_FSCTL_VOLUME_INFO.VolumeLabel` buffer given
    // there should be no null-terminator (`FSP_FSCTL_VOLUME_INFO.VolumeLabelLength` is
    // used instead).
    const VOLUME_LABEL_MAX_LEN: usize = 32;

    pub fn new(
        total_size: u64,
        free_size: u64,
        volume_label: &U16Str,
    ) -> Result<Self, VolumeLabelNameTooLong> {
        if volume_label.len() > Self::VOLUME_LABEL_MAX_LEN {
            return Err(VolumeLabelNameTooLong);
        }

        let mut vl = [0; Self::VOLUME_LABEL_MAX_LEN];
        vl[..volume_label.len()].copy_from_slice(volume_label.as_slice());

        Ok(Self(FSP_FSCTL_VOLUME_INFO {
            TotalSize: total_size,
            FreeSize: free_size,
            // It is unintuitive, but the length is in bytes, not in u16s
            VolumeLabelLength: (volume_label.len() * std::mem::size_of::<u16>()) as u16,
            VolumeLabel: vl,
        }))
    }

    pub fn total_size(&self) -> u64 {
        self.0.TotalSize
    }

    pub fn set_total_size(&mut self, size: u64) {
        self.0.TotalSize = size;
    }

    pub fn free_size(&self) -> u64 {
        self.0.FreeSize
    }

    pub fn set_free_size(&mut self, size: u64) {
        self.0.FreeSize = size;
    }

    pub fn volume_label(&self) -> &U16Str {
        let len_in_u16s = self.0.VolumeLabelLength as usize / std::mem::size_of::<u16>();
        U16Str::from_slice(&self.0.VolumeLabel[..len_in_u16s])
    }

    pub fn set_volume_label(
        &mut self,
        volume_label: &U16Str,
    ) -> Result<(), VolumeLabelNameTooLong> {
        if volume_label.len() > Self::VOLUME_LABEL_MAX_LEN {
            return Err(VolumeLabelNameTooLong);
        }

        // It is unintuitive, but the length is in bytes, not in u16s
        self.0.VolumeLabelLength = (volume_label.len() * std::mem::size_of::<u16>()) as u16;
        self.0.VolumeLabel[..volume_label.len()].copy_from_slice(volume_label.as_slice());

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CreateFileInfo {
    pub create_options: CreateOptions,
    pub granted_access: FileAccessRights,
    pub file_attributes: FileAttributes,
    pub allocation_size: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DirInfo {
    pub size: u16,
    pub file_info: FileInfo,
    _padding: [u8; 24],
    pub file_name: [u16; 255],
}

impl DirInfo {
    pub fn new(file_info: FileInfo, file_name: &U16CStr) -> Self {
        let mut buf = [0; 255];
        buf[..file_name.len()].copy_from_slice(file_name.as_slice());

        Self {
            size: (std::mem::size_of::<FSP_FSCTL_DIR_INFO>() + file_name.len() * 2) as u16,
            file_info,
            _padding: [0; 24],
            file_name: buf,
        }
    }

    pub fn from_str(file_info: FileInfo, file_name: &str) -> Self {
        let mut info = Self {
            size: 0,
            file_info,
            _padding: [0; 24],
            file_name: [0; 255],
        };

        let mut i = 0;
        for c in file_name.encode_utf16() {
            info.file_name[i] = c;
            i += 1;
        }
        info.size =
            (std::mem::size_of::<FSP_FSCTL_DIR_INFO>() + i * std::mem::size_of::<u16>()) as u16;

        info
    }

    #[cfg(windows)]
    pub fn from_osstr(file_info: FileInfo, file_name: &std::ffi::OsStr) -> Self {
        use std::os::windows::ffi::OsStrExt;

        let mut info = Self {
            size: 0,
            file_info,
            _padding: [0; 24],
            file_name: [0; 255],
        };

        let mut i = 0;
        for c in file_name.encode_wide() {
            info.file_name[i] = c;
            i += 1;
        }
        info.size =
            (std::mem::size_of::<FSP_FSCTL_DIR_INFO>() + i * std::mem::size_of::<u16>()) as u16;

        info
    }
}

#[derive(Debug, Clone, Copy)]
pub enum WriteMode {
    /// Regular write mode: start at the offset and extend the file as much as needed.
    Normal { offset: u64 },
    /// The file system must not extend the file (i.e. change the file size).
    ConstrainedIO { offset: u64 },
    /// The file system must write to the current end of file.
    WriteToEOF,
}
