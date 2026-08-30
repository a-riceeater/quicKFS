// SPDX-License-Identifier: Apache-2.0
//! The file-context representation WinFsp stores for an open handle.
//!
//! # Why this is not simply `Arc<FileContext>`
//!
//! `winfsp_wrs` already implements its `FileContextKind` trait for `Arc<T>`,
//! and using it would remove this file entirely. It cannot be used here because
//! WinFsp invokes `Flush` with a **null** file context to mean "flush the whole
//! volume", and that implementation reconstructs an `Arc` from whatever pointer
//! it is handed without checking. Reconstructing an `Arc` from null is
//! undefined behaviour, so a volume flush — which `FlushFileBuffers` on a volume
//! handle and the Volume Shadow Copy service both issue — would corrupt the
//! process.
//!
//! The alternatives were to leave `Flush` unimplemented, which makes WinFsp
//! answer every `FlushFileBuffers` with success without ever reaching the
//! server and silently drops the durability guarantee an application asked for,
//! or to represent the context as something that can be absent. This is the
//! second.
//!
//! This module is the only place in the crate that writes `unsafe`, and the
//! whole of it is the four pointer conversions below.

#![allow(unsafe_code)]

use crate::filesystem::FileContext;
use std::sync::Arc;
use winfsp_wrs::{FileContextKind, FileContextMode};
use winfsp_wrs_sys::PVOID;

/// An open handle's context, or its absence for a volume-wide operation.
#[derive(Clone)]
pub struct Handle(Option<Arc<FileContext>>);

impl Handle {
    pub fn new(context: Arc<FileContext>) -> Self {
        Self(Some(context))
    }

    /// The absent context WinFsp passes for a volume-wide operation. Exposed so
    /// tests can drive that path without a mounted volume.
    pub fn default_absent() -> Self {
        Self(None)
    }

    /// The context, if this callback was made against a file rather than the
    /// volume.
    pub fn get(&self) -> Option<&Arc<FileContext>> {
        self.0.as_ref()
    }
}

impl FileContextKind for Handle {
    const MODE: FileContextMode = FileContextMode::Descriptor;

    unsafe fn write(self, out: *mut PVOID) {
        match self.0 {
            // SAFETY: `out` is WinFsp's `PFileContext` out-parameter, valid for
            // one pointer write. The strong count moved into the raw pointer is
            // reclaimed in `access_for_close`, which WinFsp calls exactly once
            // per successful open.
            Some(context) => unsafe { out.write(Arc::into_raw(context).cast_mut().cast()) },
            // SAFETY: same out-parameter; a null context is what WinFsp itself
            // uses for "no file", so storing it is well defined.
            None => unsafe { out.write(std::ptr::null_mut()) },
        }
    }

    unsafe fn access(raw: PVOID) -> Self {
        if raw.is_null() {
            // The null context WinFsp passes for a volume-wide `Flush`. This is
            // the check whose absence made `Arc<T>` unusable here.
            return Self(None);
        }
        // SAFETY: `raw` came from `Arc::into_raw` in `write` and the allocation
        // is still live because WinFsp has not yet called `Close`. The count is
        // incremented first so the temporary `Arc` reconstructed below can be
        // dropped at the end of the callback without freeing the context.
        unsafe { Arc::increment_strong_count(raw as *const FileContext) };
        // SAFETY: as above, and the count just raised balances this reclaim.
        Self(Some(unsafe { Arc::from_raw(raw as *const FileContext) }))
    }

    unsafe fn access_for_close(raw: PVOID) -> Self {
        if raw.is_null() {
            return Self(None);
        }
        // SAFETY: `raw` came from `Arc::into_raw` in `write`. This is the
        // matching reclaim, so the count is *not* incremented: dropping the
        // returned `Handle` releases the context once the last in-flight
        // callback has finished with it.
        Self(Some(unsafe { Arc::from_raw(raw as *const FileContext) }))
    }
}
