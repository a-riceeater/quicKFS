// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! Read-only macFUSE adapter boundary. Native callback bindings are deliberately
//! gated until the macFUSE SDK is present; shared code never depends on them.
use quickfs_client_core::RemoteFilesystem;
use std::{sync::Arc, time::Duration};
pub struct Adapter {
    remote: Arc<dyn RemoteFilesystem>,
    callback_timeout: Duration,
}
impl Adapter {
    pub fn new(remote: Arc<dyn RemoteFilesystem>, callback_timeout: Duration) -> Self {
        Self {
            remote,
            callback_timeout,
        }
    }
    pub fn callback_timeout(&self) -> Duration {
        self.callback_timeout
    }
    pub fn remote(&self) -> &Arc<dyn RemoteFilesystem> {
        &self.remote
    }
}
/// Marker exposed when building macOS integration experiments. Native callback
/// bindings are intentionally absent until a maintained binding is selected.
#[cfg(all(target_os = "macos", feature = "macfuse"))]
pub const NATIVE_CALLBACKS_IMPLEMENTED: bool = false;
