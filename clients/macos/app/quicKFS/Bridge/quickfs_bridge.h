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

/// Read up to `capacity` bytes of an open stream (from a `stream_open`
/// request) at `offset` into `buffer`, storing the count in `*length`.
/// Returns NULL on success, where a short count means the end of the file,
/// or an owned JSON error reply to release with quickfs_bridge_free.
/// Blocks the calling thread until the bytes arrive.
char *quickfs_bridge_stream_read(unsigned long long stream, unsigned long long offset,
                                 unsigned char *buffer, unsigned long capacity,
                                 unsigned long *length);

#endif
