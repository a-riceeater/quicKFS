// SPDX-License-Identifier: Apache-2.0
//! The C ABI. This is the only module that may use `unsafe`: converting the
//! caller's NUL-terminated request and handing back an owned reply.
#![allow(unsafe_code)]

use std::ffi::{CStr, CString, c_char};

/// Handle one JSON request. Returns a heap-allocated, NUL-terminated JSON
/// reply that the caller must release with [`quickfs_bridge_free`]. Returns
/// null only if `request` is null.
///
/// # Safety
/// `request` must be null or point to a valid NUL-terminated string that
/// stays alive for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn quickfs_bridge_call(request: *const c_char) -> *mut c_char {
    if request.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: the caller guarantees a valid NUL-terminated string.
    let request = unsafe { CStr::from_ptr(request) }.to_string_lossy();
    let reply = std::panic::catch_unwind(|| crate::call_json(&request)).unwrap_or_else(|_| {
        r#"{"error":{"code":"panic","message":"the quicKFS core failed unexpectedly"}}"#.to_owned()
    });
    // JSON escapes control characters, so the reply never contains NUL; fall
    // back to a fixed error rather than panicking across the boundary.
    CString::new(reply)
        .unwrap_or_else(|_| {
            c"{\"error\":{\"code\":\"panic\",\"message\":\"invalid reply\"}}".to_owned()
        })
        .into_raw()
}

/// Release a reply returned by [`quickfs_bridge_call`]. Null is ignored.
///
/// # Safety
/// `value` must be null or a pointer previously returned by
/// [`quickfs_bridge_call`] that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn quickfs_bridge_free(value: *mut c_char) {
    if !value.is_null() {
        // SAFETY: the pointer came from `CString::into_raw` in this module.
        drop(unsafe { CString::from_raw(value) });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_request_through_the_c_abi() {
        let request = CString::new(r#"{"op":"environment"}"#).unwrap();
        // SAFETY: valid NUL-terminated request; the reply is freed below.
        let reply = unsafe { quickfs_bridge_call(request.as_ptr()) };
        assert!(!reply.is_null());
        // SAFETY: `reply` is a live string returned by the bridge.
        let text = unsafe { CStr::from_ptr(reply) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: freeing exactly once.
        unsafe { quickfs_bridge_free(reply) };
        assert!(text.starts_with(r#"{"ok":"#));
        // SAFETY: null is explicitly allowed.
        assert!(unsafe { quickfs_bridge_call(std::ptr::null()) }.is_null());
    }
}
