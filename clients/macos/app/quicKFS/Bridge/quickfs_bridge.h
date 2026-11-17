// SPDX-License-Identifier: Apache-2.0
// C ABI exported by the `quickfs-core-bridge` Rust crate.
#ifndef QUICKFS_BRIDGE_H
#define QUICKFS_BRIDGE_H

/// Handle one JSON request; returns an owned JSON reply (or NULL for a NULL
/// request) that must be released with quickfs_bridge_free. Blocks the
/// calling thread until the request completes.
char *quickfs_bridge_call(const char *request);

/// Release a reply returned by quickfs_bridge_call. NULL is ignored.
void quickfs_bridge_free(char *value);

#endif
