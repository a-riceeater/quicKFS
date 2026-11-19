use std::path::PathBuf;
use widestring::{U16CStr, U16CString};
use windows_sys::{w, Win32::System::LibraryLoader::LoadLibraryW};

#[derive(Debug)]
pub enum InitError {
    WinFSPNotFound,
    CannotLoadDLL { dll_path: U16CString },
}

impl std::error::Error for InitError {}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InitError::WinFSPNotFound => write!(f, "Cannot find WinFSP install directory."),
            InitError::CannotLoadDLL { dll_path } => {
                write!(f, "Cannot load WinFSP DLL {}.", dll_path.to_string_lossy())
            }
        }
    }
}

fn get_lplibfilename() -> Result<U16CString, InitError> {
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let mut path = [0u16; MAX_PATH as usize];
    let mut size = (path.len() * std::mem::size_of::<u16>()) as u32;

    let winfsp_install = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SOFTWARE\\WOW6432Node\\WinFsp"),
            w!("InstallDir"),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            path.as_mut_ptr().cast(),
            &mut size,
        )
    };

    if winfsp_install != 0 {
        return Err(InitError::WinFSPNotFound);
    }

    let path = U16CStr::from_slice(&path[0..(size as usize) / std::mem::size_of::<u16>()])
        .expect("Failed to load registry value");
    let mut path = PathBuf::from(path.to_os_string());

    path.push("bin");

    if cfg!(target_arch = "x86_64") {
        path.push("winfsp-x64.dll");
    } else if cfg!(target_arch = "x86") {
        path.push("winfsp-x86.dll");
    } else if cfg!(target_arch = "aarch64") {
        path.push("winfsp-a64.dll")
    } else {
        panic!("unsupported arch")
    }

    let path = U16CString::from_os_str(path.into_os_string()).unwrap();

    Ok(path)
}

/// This function is needed to initialize `WinFSP`.
/// You should also add `winfsp_build::build()` in your `build.rs` to allow
/// delay load, which is needed because `winfsp_wrs` depends on `WinFSP's dll`
/// which is not in Windows path or at the same location of your binary.
/// # Note: This function is idempotent, hence calling it multiple times is safe.
pub fn init() -> Result<(), InitError> {
    let dll_path = get_lplibfilename()?;
    let outcome = unsafe { LoadLibraryW(dll_path.as_ptr().cast_mut()) };
    if outcome != 0 {
        Ok(())
    } else {
        Err(InitError::CannotLoadDLL { dll_path })
    }
}
