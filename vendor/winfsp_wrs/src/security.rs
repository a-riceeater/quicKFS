use widestring::U16CStr;
use windows_sys::Win32::{
    Foundation::STATUS_SUCCESS,
    Security::{
        Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION},
        GetSecurityDescriptorLength,
    },
};
use winfsp_wrs_sys::{
    FspDeleteSecurityDescriptor, FspSetSecurityDescriptor, NTSTATUS, PSECURITY_DESCRIPTOR,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PSecurityDescriptor {
    ptr: PSECURITY_DESCRIPTOR,
    len: usize,
}

impl Default for PSecurityDescriptor {
    fn default() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
            len: 0,
        }
    }
}

impl PSecurityDescriptor {
    pub(crate) fn from_ptr(ptr: PSECURITY_DESCRIPTOR) -> Self {
        if ptr.is_null() {
            return Self {
                ptr: std::ptr::null_mut(),
                len: 0,
            };
        }

        let len = unsafe { GetSecurityDescriptorLength(ptr) as usize };

        Self { ptr, len }
    }

    pub(crate) fn inner(&self) -> PSECURITY_DESCRIPTOR {
        self.ptr
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }
}

impl From<&SecurityDescriptor> for PSecurityDescriptor {
    fn from(value: &SecurityDescriptor) -> Self {
        Self {
            ptr: value.0.as_ptr().cast_mut().cast(),
            len: value.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SecurityDescriptor(Vec<u8>);

impl SecurityDescriptor {
    fn from_ptr_and_len(ptr: PSECURITY_DESCRIPTOR, len: usize) -> Self {
        let mut handle = Vec::with_capacity(len);

        unsafe {
            std::ptr::copy(ptr as *mut u8, handle.as_mut_ptr(), len);
            handle.set_len(len);
        }

        Self(handle)
    }

    pub(crate) fn from_ptr(ptr: PSECURITY_DESCRIPTOR) -> Self {
        let len = unsafe { GetSecurityDescriptorLength(ptr) as usize };

        Self::from_ptr_and_len(ptr, len)
    }

    pub fn as_ptr(&self) -> PSecurityDescriptor {
        PSecurityDescriptor::from(self)
    }

    /// quicKFS change: the self-relative descriptor's bytes, so a file system
    /// can inspect a descriptor `set` produced (for example, to read back the
    /// rights it grants) without its own FFI copy of the structure.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn from_wstr(s: &U16CStr) -> Result<Self, String> {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0;

        unsafe {
            if {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    s.as_ptr(),
                    SDDL_REVISION,
                    &mut ptr,
                    &mut len,
                )
            } == 0
            {
                return Err(format!("Cannot create security descriptor from {s:?}"));
            }

            Ok(Self::from_ptr_and_len(ptr, len as usize))
        }
    }

    pub fn set(
        &self,
        security_information: u32,
        modification_descriptor: PSecurityDescriptor,
    ) -> Result<Self, NTSTATUS> {
        unsafe {
            let mut psd = std::ptr::null_mut();

            let status = FspSetSecurityDescriptor(
                self.as_ptr().ptr,
                security_information,
                modification_descriptor.ptr,
                &mut psd,
            );

            if status != STATUS_SUCCESS {
                return Err(status);
            }

            let sd = Self::from_ptr(psd);

            // Free psd
            FspDeleteSecurityDescriptor(
                psd,
                Some(std::mem::transmute::<
                    *const (),
                    unsafe extern "C" fn() -> NTSTATUS,
                >(FspSetSecurityDescriptor as *const ())),
            );

            Ok(sd)
        }
    }
}

impl From<PSecurityDescriptor> for SecurityDescriptor {
    fn from(value: PSecurityDescriptor) -> Self {
        Self::from_ptr_and_len(value.ptr, value.len)
    }
}
