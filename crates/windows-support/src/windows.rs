// SPDX-License-Identifier: Apache-2.0
//! The Win32 calls behind the crate's public surface.
//!
//! Every `unsafe` block in quicKFS lives in this file. Each one is preceded by
//! the reason the call is sound: which pointer is valid, who owns the
//! allocation, and where it is released.

use crate::WindowsError;
use std::{
    ffi::c_void,
    fs::File,
    os::windows::io::AsRawHandle,
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_ALREADY_EXISTS, ERROR_SUCCESS, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE,
        LocalFree, MAX_PATH, WAIT_OBJECT_0,
    },
    Security::{
        ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{
        CreateDirectoryW, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    },
    System::{
        IO::OVERLAPPED,
        Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        Threading::{
            CreateEventW, EVENT_MODIFY_STATE, GetCurrentProcess, INFINITE, OpenEventW,
            OpenProcessToken, SetEvent, WaitForSingleObject,
        },
    },
};

/// `ACCESS_ALLOWED_ACE_TYPE`; the only ACE type a private path may carry.
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

/// Trustees that are allowed to appear in a private path's DACL besides the
/// owner. Both can already take ownership of any file on the machine, so
/// refusing them would reject the DACL Windows itself creates without buying
/// any confidentiality.
const LOCAL_SYSTEM_SID: &str = "S-1-5-18";
const BUILTIN_ADMINISTRATORS_SID: &str = "S-1-5-32-544";

fn last_error(operation: &'static str) -> WindowsError {
    // SAFETY: `GetLastError` reads this thread's error slot and takes no
    // arguments.
    let code = unsafe { GetLastError() };
    WindowsError { operation, code }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// An owned self-relative security descriptor released with `LocalFree`.
struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the descriptor came from
            // `ConvertStringSecurityDescriptorToSecurityDescriptorW` or
            // `GetNamedSecurityInfoW`, both of which release with `LocalFree`.
            unsafe { LocalFree(self.0 as HLOCAL) };
        }
    }
}

/// The registry value WinFsp writes on installation.
///
/// WinFsp records its 32-bit-view install directory even in 64-bit
/// installations, which is why the `WOW6432Node` path is correct here rather
/// than a bug.
pub fn winfsp_install_directory() -> Option<PathBuf> {
    let subkey = wide(r"SOFTWARE\WOW6432Node\WinFsp");
    let value = wide("InstallDir");
    let mut buffer = [0u16; MAX_PATH as usize];
    let mut size = std::mem::size_of_val(&buffer) as u32;

    // SAFETY: `subkey` and `value` are NUL-terminated for the lifetime of the
    // call, and `size` describes `buffer` in bytes as `RegGetValueW` requires.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }

    let units = (size as usize) / std::mem::size_of::<u16>();
    let units = buffer
        .iter()
        .take(units)
        .position(|unit| *unit == 0)
        .unwrap_or(units);
    if units == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16(&buffer[..units]).ok()?))
}

/// The WinFsp user-mode DLL this build needs, inside an installation root.
fn winfsp_library_path(install: &Path) -> PathBuf {
    let library = if cfg!(target_arch = "x86_64") {
        "winfsp-x64.dll"
    } else if cfg!(target_arch = "aarch64") {
        "winfsp-a64.dll"
    } else {
        "winfsp-x86.dll"
    };
    install.join("bin").join(library)
}

/// Whether a usable WinFsp runtime is present.
///
/// Both halves matter: the registry key alone can survive a failed or partial
/// uninstall, and without the DLL the first mount would fail deep inside
/// WinFsp with a delay-load error instead of at startup with an actionable
/// message.
pub fn winfsp_is_installed() -> bool {
    winfsp_install_directory().is_some_and(|install| winfsp_library_path(&install).is_file())
}

/// The SID of the account this process runs as, in SDDL string form.
pub fn current_user_sid() -> Result<String, WindowsError> {
    let mut token: HANDLE = ptr::null_mut();
    // SAFETY: `GetCurrentProcess` returns a pseudo-handle that needs no
    // release, and `token` is a valid out-parameter.
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(last_error("OpenProcessToken"));
    }
    let token = OwnedHandle(token);

    let mut required = 0u32;
    // SAFETY: a null buffer with zero length is the documented way to ask for
    // the required size; the call is expected to fail with
    // ERROR_INSUFFICIENT_BUFFER and write `required`.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
    }
    if required == 0 {
        return Err(last_error("GetTokenInformation"));
    }

    let mut buffer = vec![0u8; required as usize];
    // SAFETY: `buffer` is `required` bytes long and stays alive for the call.
    let queried = unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    };
    if queried == 0 {
        return Err(last_error("GetTokenInformation"));
    }

    // SAFETY: on success the buffer holds a `TOKEN_USER` whose `User.Sid`
    // points into that same buffer, which outlives the read below.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
    sid_to_string(sid)
}

fn sid_to_string(sid: PSID) -> Result<String, WindowsError> {
    let mut text: *mut u16 = ptr::null_mut();
    // SAFETY: `sid` is a valid SID for the duration of the call and `text` is
    // a valid out-parameter; the returned buffer is freed by `LocalFree` below.
    let converted = unsafe { ConvertSidToStringSidW(sid, &mut text) };
    if converted == 0 {
        return Err(last_error("ConvertSidToStringSidW"));
    }
    // SAFETY: on success `text` is a NUL-terminated wide string owned by us.
    let length = unsafe { wide_len(text) };
    // SAFETY: `text` points to `length` readable units.
    let slice = unsafe { std::slice::from_raw_parts(text, length) };
    let value = String::from_utf16_lossy(slice);
    // SAFETY: `text` was allocated by `ConvertSidToStringSidW`, which
    // documents `LocalFree` as its release function, and is freed once.
    unsafe { LocalFree(text as HLOCAL) };
    Ok(value)
}

/// # Safety
/// `pointer` must be a NUL-terminated wide string.
unsafe fn wide_len(pointer: *const u16) -> usize {
    let mut length = 0;
    // SAFETY: guaranteed by the caller's contract.
    while unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    length
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: the handle came from `OpenProcessToken` and is closed
            // exactly once.
            unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
        }
    }
}

// `GetTokenInformation` lives in a module `windows-sys` splits differently
// across releases; import it once here so the call sites above stay readable.
use windows_sys::Win32::Security::GetTokenInformation;

/// The SDDL for a path only the given account may reach.
///
/// `O:` and `G:` set the owner, `D:P` marks the DACL protected so no
/// inheritable ACE from a parent directory can widen it, and the single ACE
/// grants that account full access. `OICI` makes the ACE inheritable, which
/// matters for the cache directory tree. This is the Windows equivalent of
/// `0700`/`0600` with an owner check.
fn private_sddl(sid: &str) -> String {
    format!("O:{sid}G:{sid}D:P(A;OICI;FA;;;{sid})")
}

fn security_descriptor_from_sddl(sddl: &str) -> Result<LocalSecurityDescriptor, WindowsError> {
    let text = wide(sddl);
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    let mut length = 0u32;
    // SAFETY: `text` is NUL-terminated and outlives the call; both
    // out-parameters are valid. The descriptor is released by
    // `LocalSecurityDescriptor::drop`.
    let converted = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text.as_ptr(),
            windows_sys::Win32::Security::Authorization::SDDL_REVISION_1,
            &mut descriptor,
            &mut length,
        )
    };
    if converted == 0 {
        return Err(last_error(
            "ConvertStringSecurityDescriptorToSecurityDescriptorW",
        ));
    }
    Ok(LocalSecurityDescriptor(descriptor))
}

/// Create `path` as a directory only the current account can reach.
///
/// The DACL is applied atomically at creation through `SECURITY_ATTRIBUTES`,
/// so there is no window in which the directory exists with the inherited
/// permissions of its parent. That window is exactly what the Unix
/// `DirBuilder::mode(0o700)` path avoids, and private client state must not be
/// briefly world-readable on either platform.
pub fn create_private_directory(path: &Path) -> std::io::Result<()> {
    let sid = current_user_sid()?;
    let descriptor = security_descriptor_from_sddl(&private_sddl(&sid))?;
    let attributes = windows_sys::Win32::Security::SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<windows_sys::Win32::Security::SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and `attributes` (with its descriptor)
    // stays alive across the call.
    let created = unsafe { CreateDirectoryW(wide.as_ptr(), &attributes) };
    if created == 0 {
        return Err(last_error("CreateDirectoryW").into());
    }
    Ok(())
}

/// Replace the owner and DACL of an existing file or directory so only the
/// current account can reach it.
///
/// Used where the path may already exist from an earlier run or was created by
/// a plain `create_dir_all`, which is the Windows counterpart of
/// `fs::set_permissions(path, 0o700)`.
pub fn harden_private_path(path: &Path) -> std::io::Result<()> {
    let sid = current_user_sid()?;
    let descriptor = security_descriptor_from_sddl(&private_sddl(&sid))?;

    let mut owner: PSID = ptr::null_mut();
    let mut defaulted = 0i32;
    // SAFETY: `descriptor.0` is a valid self-relative descriptor produced from
    // our own SDDL; both out-parameters are valid.
    let read_owner = unsafe {
        windows_sys::Win32::Security::GetSecurityDescriptorOwner(
            descriptor.0,
            &mut owner,
            &mut defaulted,
        )
    };
    if read_owner == 0 {
        return Err(last_error("GetSecurityDescriptorOwner").into());
    }

    let mut acl: *mut ACL = ptr::null_mut();
    let mut present = 0i32;
    let mut acl_defaulted = 0i32;
    // SAFETY: same descriptor; all out-parameters are valid.
    let read_acl = unsafe {
        windows_sys::Win32::Security::GetSecurityDescriptorDacl(
            descriptor.0,
            &mut present,
            &mut acl,
            &mut acl_defaulted,
        )
    };
    if read_acl == 0 || present == 0 {
        return Err(last_error("GetSecurityDescriptorDacl").into());
    }

    let mut wide = wide_path(path);
    // SAFETY: `wide` is NUL-terminated and mutable as the API requires; the
    // owner SID and ACL point into `descriptor`, which outlives the call.
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide.as_mut_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            ptr::null_mut(),
            acl,
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// Whether `path` is reachable only by the current account.
///
/// This is the Windows analogue of the Unix `uid == geteuid() && mode & 0o077
/// == 0` check the trust store and cache already perform: the owner must be
/// this account, the DACL must be protected against inherited ACEs, and every
/// allow-ACE must name this account, `LocalSystem`, or `BUILTIN\Administrators`.
/// Those two are permitted because they can take ownership of any object
/// regardless, so excluding them would reject correct DACLs without adding any
/// confidentiality.
pub fn validate_private_owner(path: &Path) -> std::io::Result<bool> {
    let expected = current_user_sid()?;
    let wide = wide_path(path);

    let mut owner: PSID = ptr::null_mut();
    let mut dacl: *mut ACL = ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = ptr::null_mut();
    // SAFETY: `wide` is NUL-terminated; every out-parameter is valid. On
    // success `descriptor` owns the memory `owner` and `dacl` point into and
    // is released by `LocalSecurityDescriptor::drop` below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32));
    }
    let descriptor = LocalSecurityDescriptor(descriptor);

    if sid_to_string(owner)? != expected {
        return Ok(false);
    }

    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor.0` is valid and both out-parameters are valid.
    let read_control =
        unsafe { GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) };
    if read_control == 0 {
        return Err(last_error("GetSecurityDescriptorControl").into());
    }
    if control & SE_DACL_PROTECTED == 0 {
        return Ok(false);
    }

    // A null DACL grants everyone everything; an empty one grants nobody
    // anything. Only the second is acceptable.
    if dacl.is_null() {
        return Ok(false);
    }

    // SAFETY: `dacl` points into `descriptor`, which is still alive.
    let count = unsafe { (*dacl).AceCount } as u32;
    for index in 0..count {
        let mut ace: *mut c_void = ptr::null_mut();
        // SAFETY: `index` is below the ACE count reported by the same ACL.
        if unsafe { GetAce(dacl, index, &mut ace) } == 0 {
            return Err(last_error("GetAce").into());
        }
        // SAFETY: `ace` points at an `ACE_HEADER` inside the live ACL.
        let kind = unsafe { (*ace.cast::<windows_sys::Win32::Security::ACE_HEADER>()).AceType };
        if kind != ACCESS_ALLOWED_ACE_TYPE {
            // Deny ACEs only ever narrow access, and audit ACEs live in the
            // SACL, so anything that is not an allow-ACE cannot widen this
            // path. Treat it as acceptable rather than failing a mount over a
            // policy-installed deny rule.
            continue;
        }
        // SAFETY: an allow-ACE is an `ACCESS_ALLOWED_ACE` whose `SidStart`
        // field begins the trustee SID; taking its address yields that SID.
        let trustee = unsafe {
            ptr::addr_of!(
                (*ace.cast::<windows_sys::Win32::Security::ACCESS_ALLOWED_ACE>()).SidStart
            ) as PSID
        };
        let trustee = sid_to_string(trustee)?;
        if trustee != expected
            && trustee != LOCAL_SYSTEM_SID
            && trustee != BUILTIN_ADMINISTRATORS_SID
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Take an exclusive, non-blocking lock on the whole of `file`.
///
/// The Unix path uses `flock(LOCK_EX | LOCK_NB)` to stop two clients sharing
/// one cache namespace; `LockFileEx` with the same two flags is the Windows
/// equivalent. The lock is released when the handle closes, so callers keep
/// the `File` alive for as long as they hold the namespace.
pub fn lock_file(file: &File) -> Result<(), WindowsError> {
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is owned by `file` and outlives the call; the range
    // covers the whole file; `overlapped` is a valid zeroed structure.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle() as HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if locked == 0 {
        return Err(last_error("LockFileEx"));
    }
    Ok(())
}

/// A named, session-local event one process uses to ask another to unmount.
///
/// A WinFsp mount lives inside the process that started it: there is no service
/// to ask, and nothing outside that process can take the volume down cleanly.
/// `quickfs unmount Q:` therefore has to reach the mounting process, and a named
/// event is the smallest Windows primitive that does it — no port to bind, no
/// pipe to secure, and the object disappears with the last handle so a crashed
/// mount leaves nothing behind to clean up.
///
/// The name lives in the `Local\` namespace, which is per logon session. That
/// is the right scope: a mount is visible only to the session that created it,
/// so only that session should be able to take it down.
pub struct UnmountSignal {
    handle: OwnedHandle,
}

// SAFETY: a Windows event `HANDLE` is a process-wide kernel object reference,
// not thread-affine; every operation on it (`WaitForSingleObject`, `SetEvent`,
// `CloseHandle`) is documented as callable from any thread. The mount CLI moves
// one to a waiter thread, which is why this is needed at all.
unsafe impl Send for UnmountSignal {}

fn unmount_event_name(identifier: &str) -> Vec<u16> {
    // Only the characters `MountPoint::identifier` produces reach this, so the
    // name cannot contain a backslash and escape the `Local\` namespace.
    wide(&format!(r"Local\quickfs-unmount-{identifier}"))
}

impl UnmountSignal {
    /// Create the event for a new mount.
    ///
    /// Fails with `ERROR_ALREADY_EXISTS` when this session already has a mount
    /// on the same mountpoint, which is exactly the check that stops two
    /// clients fighting over one drive letter.
    pub fn create(identifier: &str) -> Result<Self, WindowsError> {
        let name = unmount_event_name(identifier);
        // SAFETY: `name` is NUL-terminated and outlives the call; the null
        // security attributes give the event this session's default DACL.
        let handle = unsafe {
            CreateEventW(
                ptr::null(),
                1, // manual reset: a waiter that arrives late still sees it
                0, // initially unsignalled
                name.as_ptr(),
            )
        };
        if handle.is_null() {
            return Err(last_error("CreateEventW"));
        }
        let handle = OwnedHandle(handle);
        // SAFETY: reads this thread's error slot, set by the call above.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return Err(WindowsError {
                operation: "CreateEventW",
                code: ERROR_ALREADY_EXISTS,
            });
        }
        Ok(Self { handle })
    }

    /// Block until another process asks this mount to stop.
    pub fn wait(&self) -> Result<(), WindowsError> {
        // SAFETY: the handle is owned by `self` and outlives the call.
        let outcome = unsafe { WaitForSingleObject(self.handle.0, INFINITE) };
        if outcome == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(last_error("WaitForSingleObject"))
        }
    }
}

/// Ask the process holding `identifier` to unmount.
///
/// Returns `false` when no mount by that name exists in this session, which the
/// CLI reports as "nothing is mounted there" rather than as a failure.
pub fn signal_unmount(identifier: &str) -> Result<bool, WindowsError> {
    let name = unmount_event_name(identifier);
    // SAFETY: `name` is NUL-terminated and outlives the call.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        return Ok(false);
    }
    let handle = OwnedHandle(handle);
    // SAFETY: the handle was opened with `EVENT_MODIFY_STATE` just above.
    if unsafe { SetEvent(handle.0) } == 0 {
        return Err(last_error("SetEvent"));
    }
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_current_account_sid_is_well_formed() {
        let sid = current_user_sid().unwrap();
        assert!(sid.starts_with("S-1-"), "unexpected SID form: {sid}");
        assert!(private_sddl(&sid).contains(&sid));
    }

    #[test]
    fn a_created_private_directory_validates_and_a_default_one_does_not() {
        let root = tempfile::tempdir().unwrap();
        let private = root.path().join("private");
        create_private_directory(&private).unwrap();
        assert!(validate_private_owner(&private).unwrap());

        // A directory created with inherited permissions has an unprotected
        // DACL, which is exactly the state the validator must reject.
        let inherited = root.path().join("inherited");
        std::fs::create_dir(&inherited).unwrap();
        assert!(!validate_private_owner(&inherited).unwrap());

        // Hardening repairs it in place.
        harden_private_path(&inherited).unwrap();
        assert!(validate_private_owner(&inherited).unwrap());
    }

    #[test]
    fn one_holder_at_a_time_may_lock_a_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("namespace.lock");
        let first = File::create(&path).unwrap();
        lock_file(&first).unwrap();
        let second = File::options().read(true).write(true).open(&path).unwrap();
        assert!(lock_file(&second).is_err());
        drop(first);
        assert!(lock_file(&second).is_ok());
    }

    #[test]
    fn one_mount_at_a_time_may_claim_a_mountpoint_and_a_signal_reaches_it() {
        let identifier = format!("TEST{}", std::process::id());
        assert!(
            !signal_unmount(&identifier).unwrap(),
            "signalling a mountpoint nothing holds must report that, not fail"
        );

        let signal = UnmountSignal::create(&identifier).unwrap();
        // A second mount on the same mountpoint has to be refused, or two
        // clients would fight over one drive letter.
        assert!(UnmountSignal::create(&identifier).is_err());

        assert!(signal_unmount(&identifier).unwrap());
        signal.wait().unwrap();

        drop(signal);
        assert!(
            !signal_unmount(&identifier).unwrap(),
            "the event must not outlive the mount that created it"
        );
    }

    #[test]
    fn the_runtime_probe_agrees_with_the_registry_and_the_library() {
        // Whichever way this machine is configured, the two halves of the
        // probe must agree: claiming an installation without the DLL present
        // is the failure mode this check exists to prevent.
        match winfsp_install_directory() {
            Some(install) => {
                assert_eq!(
                    winfsp_is_installed(),
                    winfsp_library_path(&install).is_file()
                );
            }
            None => assert!(!winfsp_is_installed()),
        }
    }
}
