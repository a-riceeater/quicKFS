# Graph Report - quicKFS  (2026-08-30)

## Corpus Check
- 207 files · ~183,031 words
- Verdict: corpus is large enough that graph structure adds value.

## Summary
- 3262 nodes · 11051 edges · 156 communities (134 shown, 22 thin omitted)
- Extraction: 99% EXTRACTED · 1% INFERRED · 0% AMBIGUOUS · INFERRED: 161 edges (avg confidence: 0.81)
- Token cost: 231,820 input · 0 output

## Community Hubs (Navigation)
- Client CLI and Trust Commands
- QUIC Transport Layer
- fuser simple.rs Example FS
- CachedFilesystem Implementation
- Server Export Session
- fuser Example Scaffolding
- Low-Level Reply Buffers
- Auth Users and Pairing
- macFUSE Adapter Callbacks
- fuser Notify and Poll Examples
- FUSE Request ABI Structs
- Client Cache Invalidation
- macFUSE Flag Conversion
- NetworkFilesystem Client
- Cached Filesystem Tests
- Linux Server Daemon CLI
- fuser Reply Senders
- Server Directory and Metadata Ops
- fuser Async Experimental API
- Cache Namespace and Validation
- Media Workload Smoke Script
- Server Core Integration Tests
- FUSE ABI Struct Definitions
- fuser Session Lifecycle
- macFUSE Mock Filesystem Tests
- fuser Request Parsing
- fusermount Mount Helper
- fuser Reply Types
- macFUSE Inode and Directory Cache
- IoSlice Concatenation
- Kernel Capability Config
- Resilient Filesystem Wrapper
- macFUSE Async Execution Bridge
- Export Discovery and Listing
- macFUSE Adapter State
- Hot Range Cache Tier
- Cache Tier Containers
- Server Limits and Node Keys
- Persistent Cache Manifest
- Protocol Encoding Tests
- macFUSE Mount Lifecycle
- Deferred Cache Commits
- GUI npm Dependencies
- macFUSE Async Mutations
- Resilient Write and Lock Ops
- Server Connection Auth Handling
- Kernel Cache Notifications
- fuser Passthrough Example
- fuser Channel and Dispatch
- fuser Errno Mapping
- Client Connection Slots
- Reconnect and Lock History Tests
- Tauri App Configuration
- GUI TypeScript Config
- Low-Level Notify Encoding
- Protocol Frame Codec
- Protocol Request Identifiers
- Server Copy-Range and Xattrs
- FUSE Setattr Handling
- Mount and Unmount Syscalls
- fuse3 C Type Bindings
- Directory View Wire Types
- /dev/fuse File Descriptor
- Ioctl Flags
- Release Flags
- Server State Initialization
- Caching Design Rationale
- GUI Pairing Code and Theme
- Reconnect Policy Configuration
- FUSE Argument Iterator
- Read Request Flags
- fuse2 Mount Implementation
- Directory Invalidation Protocol
- Request Header Accessors
- Rename and Link Requests
- Client Directory View Chunking
- macFUSE Install Detection
- Authentication and Trust Design
- File Lock Requests
- Reconnect Failure State
- Server Attribute Application
- Workspace Crate Manifests
- Auth Identity Lock
- Common Path Validation
- Trust State and Certificates
- fuse3 Mount Implementation
- GUI Shell and Export Confinement
- Aligned Data Wrappers
- Security Audit Findings
- Repo Contribution Templates
- Cache Tiering Backlog
- Poll Flags
- Finder Mount Race Investigation
- WAN Read-Ahead Investigation
- Batch Forget Requests
- Poll Event Replies
- Platform Ports and CI Jobs
- Node Identity and Reconnect Invariants
- Tauri Capability Permissions
- GUI Tauri Backend Commands
- Test Support Fixtures
- fuser Build and Quality Gates
- Write Request Accessors
- macFUSE Xattr Operations
- Getxattr Requests
- fuser Request Parameters
- Crate and Platform Boundaries
- Offline Write Policy
- CLI Usage Documentation
- libfuse Build Script
- Directory Entry Replies
- Cuse Init Flags
- Fsync Flags
- FUSE Read Buffer
- Postcard Versioning Rules
- Getattr Flags
- Poll Client Example
- pjdfs Test Harness
- Access Flags
- Lseek Requests
- Create Requests
- Mknod Requests
- Symlink and Volname Requests
- Setxattr Requests
- BSD pjdfs Harness
- macOS pjdfs Harness
- xfstests Harness
- fuser macOS CI Workflows
- Flush Requests
- Lookup and Rmdir Requests
- Mkdir Requests
- Filesystem Holder Drop
- Vite Environment Types
- Quality Check Script
- Dev Certificate Script
- Test Runner Script
- Ioctl Backing Map
- Removexattr Requests
- Unlink Requests
- Passthrough Test Script
- Test Support Crate

## God Nodes (most connected - your core abstractions)
1. `NodeId` - 245 edges
2. `INodeNo` - 192 edges
3. `ServerError` - 136 edges
4. `Name` - 116 edges
5. `Adapter` - 102 edges
6. `Metadata` - 102 edges
7. `AdapterError` - 101 edges
8. `ExportSession` - 98 edges
9. `ResilientFilesystem` - 71 edges
10. `CachedFilesystem` - 69 edges

## Surprising Connections (you probably didn't know these)
- `Single-Flight range_fetches Gate` --semantically_similar_to--> `Native Directory Projection and Single-Flight Cold Load`  [INFERRED] [semantically similar]
  b.md → ARCHITECTURE.md
- `main()` --calls--> `init_logging()`  [INFERRED]
  servers/linux/server-daemon/src/main.rs → crates/common/src/lib.rs
- `Contributing Guide` --conceptually_related_to--> `Linux CI Job`  [INFERRED]
  CONTRIBUTING.md → .github/workflows/ci.yml
- `Local Quality Gate (scripts/check.sh)` --semantically_similar_to--> `fuser CI Workflow`  [INFERRED] [semantically similar]
  docs/development.md → vendor/fuser/.github/workflows/ci.yml
- `pair()` --calls--> `verify_pairing()`  [INFERRED]
  clients/macos/client-cli/src/main.rs → crates/client-core/src/lib.rs

## Import Cycles
- 1-file cycle: `vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/request.rs`
- 2-file cycle: `vendor/fuser/src/mnt/mount_options.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/mnt/mount_options.rs`
- 2-file cycle: `vendor/fuser/src/mnt/mod.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/mnt/mod.rs`
- 2-file cycle: `vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs`
- 2-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/reply.rs -> vendor/fuser/src/lib.rs`
- 2-file cycle: `vendor/fuser/src/ll/mod.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/mod.rs`
- 2-file cycle: `vendor/fuser/src/ll/request.rs -> vendor/fuser/src/notify.rs -> vendor/fuser/src/ll/request.rs`
- 2-file cycle: `vendor/fuser/src/ll/notify.rs -> vendor/fuser/src/notify.rs -> vendor/fuser/src/ll/notify.rs`
- 3-file cycle: `vendor/fuser/src/mnt/mod.rs -> vendor/fuser/src/mnt/mount_options.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/mnt/mod.rs`
- 3-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 3-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/reply.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 3-file cycle: `vendor/fuser/src/ll/mod.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/ll/mod.rs`
- 3-file cycle: `vendor/fuser/src/ll/notify.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/notify.rs -> vendor/fuser/src/ll/notify.rs`
- 4-file cycle: `vendor/fuser/src/forget_one.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs -> vendor/fuser/src/forget_one.rs`
- 4-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 4-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/reply.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 5-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/mnt/mount_options.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 5-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/session.rs -> vendor/fuser/src/notify.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`
- 5-file cycle: `vendor/fuser/src/lib.rs -> vendor/fuser/src/reply.rs -> vendor/fuser/src/ll/mod.rs -> vendor/fuser/src/ll/request.rs -> vendor/fuser/src/ll/reply.rs -> vendor/fuser/src/lib.rs`

## Hyperedges (group relationships)
- **Server Trust and User Authentication Flow** — readme_server_trust_modes, readme_pairing_assisted_exact_pin, readme_authentication_pipeline, security_audit_authorization_chain, security_security_policy [EXTRACTED 1.00]
- **Protocol 6.x Additive Minor Feature Family** — changelog_version_negotiation, changelog_minor_6_1_directory_pagination, changelog_minor_6_3_frame_compression, changelog_minor_6_4_metadata_batch, contributing_postcard_versioning_rule [EXTRACTED 1.00]
- **High-Latency Read Performance Program** — b_adaptive_read_ahead, b_sequentialprefetcher, changelog_video_stutter_fix, changelog_prefetch_convoy_cap, changelog_first_open_priming [INFERRED 0.85]
- **Four Interchangeable Server-Trust Modes** — docs_authentication_pairing_assisted_tofu, docs_authentication_managed_exact_pin, docs_authentication_enterprise_ca_mode, docs_authentication_system_roots_mode, docs_authentication_trust_policy_preflight [EXTRACTED 1.00]
- **Adaptive Prefetch Control Loop** — docs_caching_adaptive_read_ahead, docs_caching_multi_stream_per_handle, docs_caching_stall_driven_window_sizing, docs_caching_cold_start_priming, docs_caching_buffer_depth_vs_fetch_concurrency, docs_caching_noreadahead_mount_option [EXTRACTED 1.00]
- **Postcard-Constrained Minor Compatibility Regime** — docs_protocol_spec_postcard_serialization, docs_protocol_spec_version_negotiation, docs_protocol_spec_capability_gating, docs_protocol_spec_adding_a_version_guideline, docs_protocol_spec_non_additive_changes, docs_protocol_frame_compression, docs_protocol_metadata_batching, docs_protocol_streamed_directory_view [EXTRACTED 1.00]

## Communities (156 total, 22 thin omitted)

### Community 0 - "Client CLI and Trust Commands"
Cohesion: 0.06
Nodes (78): Cli, Command, ensure_unpaired(), forget(), import_trust(), load_trust(), lock_trust_store(), main() (+70 more)

### Community 1 - "QUIC Transport Layer"
Cohesion: 0.07
Nodes (60): AtomicU16, ClientConfig, ClosedStream, Connection, ConnectionError, Duration, certificate_sha256(), filesystem_transport_config() (+52 more)

### Community 2 - "fuser simple.rs Example FS"
Cohesion: 0.10
Nodes (38): BTreeMap, DirectoryDescriptor, Args, as_file_kind(), check_access(), clear_suid_sgid(), creation_gid(), FileKind (+30 more)

### Community 3 - "CachedFilesystem Implementation"
Cohesion: 0.08
Nodes (19): CachedFilesystem, CachedHandle, filesystem_stats(), is_offline(), require_online_handle(), AtomicBool, AtomicU64, AtomicUsize (+11 more)

### Community 4 - "Server Export Session"
Cohesion: 0.12
Nodes (22): blocking(), ExportSession, is_resource_fork(), lock_mutex(), logical_xattr_name(), OpenFile, AsyncMutex, AtomicBool (+14 more)

### Community 5 - "fuser Example Scaffolding"
Cohesion: 0.05
Nodes (49): add_directory_row(), file_type(), fill_directory(), CommonArgs, PathBuf, Args, HelloFS, AtomicU64 (+41 more)

### Community 6 - "Low-Level Reply Buffers"
Cohesion: 0.06
Nodes (43): ResponseBuf, S, Generation, Attr, DirEntList, DirEntPlusList, DirEntryPlus, DirEntryPlus<T> (+35 more)

### Community 7 - "Auth Users and Pairing"
Cohesion: 0.13
Nodes (58): add_user(), AuthError, change_password(), consume_pairing(), create_pairing(), format_code(), grouping_preserves_url_safe_hyphens_in_pairing_secret(), hash_password() (+50 more)

### Community 8 - "macFUSE Adapter Callbacks"
Cohesion: 0.14
Nodes (22): Adapter, add_directory_plus_row(), errno(), file_attr(), fill_directory_plus(), permission_mode(), remove_callback(), rename_callback() (+14 more)

### Community 9 - "fuser Notify and Poll Examples"
Cohesion: 0.09
Nodes (31): LowerHex, UpperHex, ClockFS, main(), now_string(), Arc, AtomicU64, FileHandle (+23 more)

### Community 10 - "FUSE Request ABI Structs"
Cohesion: 0.12
Nodes (42): fuse_bmap_in, fuse_fsync_in, fuse_getxattr_in, fuse_in_header, fuse_opcode, fuse_open_in, Access, BatchForget (+34 more)

### Community 11 - "Client Cache Invalidation"
Cohesion: 0.10
Nodes (6): DelayedFilesystem<T>, DirectoryEntry, FileHandle, Vec, XattrRead, NodeId

### Community 12 - "macFUSE Flag Conversion"
Cohesion: 0.06
Nodes (43): Adapter, backend_mount_option(), client_errno(), converts_open_flags_without_losing_append_or_truncate(), converts_posix_lock_types_and_rejects_invalid_ranges(), force_local_unmount(), is_expected_lookup_miss(), lock_type() (+35 more)

### Community 13 - "NetworkFilesystem Client"
Cohesion: 0.14
Nodes (9): NetworkFilesystem, response_error(), Option, RecvStream, Request, Response, Result, Self (+1 more)

### Community 14 - "Cached Filesystem Tests"
Cohesion: 0.11
Nodes (38): cached_directory_is_returned_while_one_remote_refresh_runs(), cached_ranges_and_directory_snapshots_work_offline(), CachePolicy, ClientError, cold_open_primes_read_ahead_and_arms_a_deep_window(), concurrent_failed_reads_share_one_remote_fetch(), concurrent_overlapping_reads_share_one_remote_block_fetch(), controller_detects_pattern_and_resets_on_revision_change() (+30 more)

### Community 15 - "Linux Server Daemon CLI"
Cohesion: 0.10
Nodes (39): resolve_path(), Init, authentication_rate_limit_is_scoped_by_peer(), Cli, client_metadata_batch_negotiates_and_maps_results(), Command, directory_view_compression_is_negotiated_by_minor_version(), enterprise_ca_authenticates_server_without_pairing() (+31 more)

### Community 16 - "fuser Reply Senders"
Cohesion: 0.10
Nodes (25): SyncSender, AssertSender, async_reply(), Data, reply_attr(), reply_bmap(), reply_data(), reply_directory() (+17 more)

### Community 17 - "Server Directory and Metadata Ops"
Cohesion: 0.12
Nodes (34): DirectoryEntry, DirectoryViewOptions, Metadata, NodeKind, directories_symlinks_removal_and_real_revisions_work(), enrich_resource_fork_revision_at(), identity(), identity_from_stat() (+26 more)

### Community 18 - "fuser Async Experimental API"
Cohesion: 0.08
Nodes (28): RequestContext, Args, HelloFS, FileHandle, Option, OsStr, Result, Vec (+20 more)

### Community 19 - "Cache Namespace and Validation"
Cohesion: 0.14
Nodes (28): acquire_namespace_lock(), CacheError, CacheStats, create_private_directory(), ensure_private_directory(), evict_to_budget(), load_manifest(), metadata() (+20 more)

### Community 20 - "Media Workload Smoke Script"
Cohesion: 0.12
Nodes (30): Namespace, RuntimeError, DarwinAttrList, DarwinTimespec, exchange_data(), get_backup_time(), get_xattr(), list_xattrs() (+22 more)

### Community 21 - "Server Core Integration Tests"
Cohesion: 0.15
Nodes (31): append_allocation_stats_and_external_revisions_work(), byte_range_locks_conflict_split_and_cleanup_across_sessions(), directory_view_beyond_one_frame_projects_every_entry(), enriched_directory_view_batches_metadata_and_bounded_xattrs(), forgotten_nodes_release_server_registry_capacity(), handles_remain_session_scoped_and_globally_bounded(), hardlinks_copy_seek_ioctl_bmap_and_exchangedata_work(), metadata_batch_discovers_unknown_nodes_from_a_fresh_registry() (+23 more)

### Community 22 - "FUSE ABI Struct Definitions"
Cohesion: 0.06
Nodes (41): FattrFlags, cuse_init_in, cuse_init_out, fuse_access_in, fuse_attr, fuse_attr_out, fuse_batch_forget_in, fuse_bmap_out (+33 more)

### Community 23 - "fuser Session Lifecycle"
Cohesion: 0.12
Nodes (24): OwnedFd, Config, Option, BackgroundSession, FilesystemHolder, Arc, AsFd, BorrowedFd (+16 more)

### Community 24 - "macFUSE Mock Filesystem Tests"
Cohesion: 0.14
Nodes (13): ClientResult, directory_node(), file_node(), metadata(), MockFilesystem, not_found(), remote_handle(), AtomicUsize (+5 more)

### Community 25 - "fuser Request Parsing"
Cohesion: 0.09
Nodes (20): fuse_fallocate_in, fuse_init_in, AnyRequest, CopyFileRangeFile, FAllocate, FileHandle, Init, RequestError (+12 more)

### Community 26 - "fusermount Mount Helper"
Cohesion: 0.13
Nodes (32): ErrorKind, MntFlags, MsFlags, UnixStream, clear_cloexec_in_pre_exec(), detect_fusermount_bin(), fuse_mount_fusermount(), fuse_mount_mount_fusefs() (+24 more)

### Community 27 - "fuser Reply Types"
Cohesion: 0.09
Nodes (19): Reply, reply_create(), reply_open(), ReplyBmap, ReplyCreate, ReplyIoctl, ReplyLock, ReplyLseek (+11 more)

### Community 28 - "macFUSE Inode and Directory Cache"
Cohesion: 0.16
Nodes (11): AdapterError, DirectoryHandleRecord, DirectoryListing, LookupResult, Arc, Error, HashSet, Result (+3 more)

### Community 29 - "IoSlice Concatenation"
Cohesion: 0.09
Nodes (20): Iterator, fuse_ioctl_out, (A, B), &[IoSlice<'_>], [IoSlice<'_>; N], IosliceConcat, Option<A>, Item (+12 more)

### Community 30 - "Kernel Capability Config"
Cohesion: 0.10
Nodes (17): add_capability_if_supported(), default_init_flags(), KernelConfig, mount2(), Duration, FS, Option, P (+9 more)

### Community 31 - "Resilient Filesystem Wrapper"
Cohesion: 0.11
Nodes (6): capability_epoch(), is_operation_timeout(), DirectoryEntry, F, Result, Vec

### Community 33 - "Export Discovery and Listing"
Cohesion: 0.11
Nodes (30): BTreeSet, discover_targets(), DiscoveryContext, ExportShared, FileIdentity, first_conflict(), KnownNode, LockRecord (+22 more)

### Community 34 - "macFUSE Adapter State"
Cohesion: 0.09
Nodes (29): AdapterState, CachedDirectory, CachedMetadata, CachedXattrs, case_insensitive_lookup_normalizes_unicode_and_rejects_ambiguity(), CreatedAndOpenedFile, DirectoryEntry, FileHandleRecord (+21 more)

### Community 35 - "Hot Range Cache Tier"
Cohesion: 0.19
Nodes (17): assemble_range(), Assembled, hot_range_tier_evicts_least_recently_used_blocks_to_its_byte_budget(), HotRangeCache, HotRangeState, RangeKey, ranges_overlap(), RevisionKey (+9 more)

### Community 36 - "Cache Tier Containers"
Cohesion: 0.14
Nodes (18): DirectoryCache, FilesystemStateCache, MemoryCache, MetadataCache, NodeCacheInvalidation, NonBlockingPersistentCache, PersistentWrite, RangeCache (+10 more)

### Community 37 - "Server Limits and Node Keys"
Cohesion: 0.13
Nodes (25): Limits, Default, Duration, Self, decode_node_key(), exchange_resource_forks(), Export, export_marker() (+17 more)

### Community 38 - "Persistent Cache Manifest"
Cohesion: 0.17
Nodes (26): CacheNamespace, corrupt_payload_is_rejected_and_removed_from_manifest(), deferred_commit_removes_replaced_payloads_at_flush(), deferred_commits_coalesce_manifest_writes(), deferred_flush_is_a_no_op_when_clean(), directory_entry(), directory_snapshot_batches_child_metadata_and_survives_reopen(), identical_persistent_insert_is_a_noop() (+18 more)

### Community 39 - "Protocol Encoding Tests"
Cohesion: 0.10
Nodes (21): accepts_same_major_any_minor_but_rejects_other_major(), enriched_directory_request_round_trips_with_bounded_options(), Envelope<T>, FileHandle, LockKind, make_version(), max_metadata_batch_response_fits_one_frame(), metadata_batch_request_and_response_round_trip() (+13 more)

### Community 40 - "macFUSE Mount Lifecycle"
Cohesion: 0.19
Nodes (17): arbitrary_range_read_is_split_at_negotiated_limit(), callback_deadline_bounds_remote_work_on_the_shared_runtime(), concurrent_cold_directory_callbacks_share_one_enriched_request(), concurrent_writes_cannot_regress_the_local_handle_revision(), destroy_is_safe_when_invoked_from_a_runtime_worker(), file_mutation_invalidates_parent_view_that_embeds_old_child_metadata(), finder_style_lookup_getattr_and_snapshot_readdir_use_stable_inodes(), forget_and_batch_forget_evict_inodes_and_notify_the_remote_session() (+9 more)

### Community 41 - "Deferred Cache Commits"
Cohesion: 0.14
Nodes (11): DeferredCommits, DiskEntry, EntryKey, next_clock(), PersistentCache, PersistentState, AtomicU64, Fn (+3 more)

### Community 42 - "GUI npm Dependencies"
Cohesion: 0.07
Nodes (27): dependencies, svelte, @tauri-apps/api, devDependencies, svelte-check, @sveltejs/vite-plugin-svelte, @tauri-apps/cli, typescript (+19 more)

### Community 43 - "macFUSE Async Mutations"
Cohesion: 0.24
Nodes (6): CreatedNode, validate_metadata(), validate_name(), Name, RenameMode, Display

### Community 44 - "Resilient Write and Lock Ops"
Cohesion: 0.21
Nodes (7): FileRevision, WriteResult, FileHandle, Future, Option, Output, FileLock

### Community 45 - "Server Connection Auth Handling"
Cohesion: 0.13
Nodes (27): IpAddr, authentication_error(), AuthRateLimiter, ConnectionAuth, directory_listing_response(), handle(), internal_error(), RequestContext (+19 more)

### Community 46 - "Kernel Cache Notifications"
Cohesion: 0.15
Nodes (13): NotificationBuf, notify_code, Notification, Notifier, PollHandle, PollNotifier, Debug, Error (+5 more)

### Community 47 - "fuser Passthrough Example"
Cohesion: 0.14
Nodes (16): BackingCache, main(), PassthroughFs, Arc, FileHandle, Fn, HashMap, Mutex (+8 more)

### Community 48 - "fuser Channel and Dispatch"
Cohesion: 0.13
Nodes (13): Channel, ChannelSender, Arc, AsFd, BorrowedFd, Result, Self, RequestWithSender (+5 more)

### Community 49 - "fuser Errno Mapping"
Cohesion: 0.13
Nodes (6): Errno, i32, Error, From, Self, u64

### Community 50 - "Client Connection Slots"
Cohesion: 0.16
Nodes (16): OpenedFile, RemoteFilesystem, Send, Sync, ConnectionSlot, HandleState, QueueConnector, ResilientFilesystem (+8 more)

### Community 51 - "Reconnect and Lock History Tests"
Cohesion: 0.24
Nodes (20): apply_lock_to_history(), blocked_operation_returns_at_deadline(), concurrent_requests_share_a_failed_reconnect_batch(), connector(), deadline_options(), history_lock(), lock_history_compacts_instead_of_growing_or_discarding(), never_retries_an_ambiguous_write() (+12 more)

### Community 52 - "Tauri App Configuration"
Cohesion: 0.08
Nodes (23): app, security, windows, build, beforeBuildCommand, beforeDevCommand, devUrl, frontendDist (+15 more)

### Community 53 - "GUI TypeScript Config"
Cohesion: 0.08
Nodes (23): compilerOptions, allowJs, checkJs, esModuleInterop, forceConsistentCasingInFileNames, isolatedModules, lib, module (+15 more)

### Community 54 - "Low-Level Notify Encoding"
Cohesion: 0.22
Nodes (12): fuse_notify_code, delete(), inval_entry(), inval_inode(), Notification<'a>, poll(), F, OsStr (+4 more)

### Community 55 - "Protocol Frame Codec"
Cohesion: 0.20
Nodes (18): T, chunk_filled_to_budget_encodes_within_frame_limit(), CodecError, compress_flag_gates_emission_but_not_decoding(), decode(), decode_frame(), decode_request(), encode() (+10 more)

### Community 56 - "Protocol Request Identifiers"
Cohesion: 0.13
Nodes (8): RequestId, Debug, Default, From, Self, String, SecretProof, SecretString

### Community 57 - "Server Copy-Range and Xattrs"
Cohesion: 0.22
Nodes (20): claim_inline_budget(), copy_range_buffered(), copy_range_buffered_overlap(), copy_range_in_chunks(), copy_range_server(), exchange_file_contents(), list_fd_xattrs(), open_directory() (+12 more)

### Community 58 - "FUSE Setattr Handling"
Cohesion: 0.23
Nodes (6): fuse_setattr_in, BsdFileFlags, Option, SystemTime, SetAttr, system_time_from_time()

### Community 59 - "Mount and Unmount Syscalls"
Cohesion: 0.17
Nodes (17): cmd_mount(), fuse_args(), is_mounted(), libc_umount(), Mount, mount_unmount(), MountImpl, Arc (+9 more)

### Community 60 - "fuse3 C Type Bindings"
Cohesion: 0.19
Nodes (19): c_uint, dev_t, fuse_bufvec, fuse_forget_data, fuse_ino_t, fuse_pollhandle, fuse_req_t, mode_t (+11 more)

### Community 61 - "Directory View Wire Types"
Cohesion: 0.16
Nodes (17): view_entry(), AttributeChanges, BatchedMetadata, DirectoryView, DirectoryViewEntry, ErrorCode, frame_body(), InlineXattr (+9 more)

### Community 62 - "/dev/fuse File Descriptor"
Cohesion: 0.15
Nodes (13): AsRawFd, RawFd, DevFuse, AsFd, BorrowedFd, File, Result, Self (+5 more)

### Community 63 - "Ioctl Flags"
Cohesion: 0.11
Nodes (9): IoctlFlags, Display, Formatter, Result, fuse_copy_file_range_in, fuse_ioctl_in, CopyFileRange, IoCtl (+1 more)

### Community 64 - "Release Flags"
Cohesion: 0.14
Nodes (7): ReleaseFlags, Display, Formatter, Result, fuse_release_in, Release, ReleaseDir

### Community 65 - "Server State Initialization"
Cohesion: 0.19
Nodes (17): certificate_fingerprint(), creates_private_state_and_rejects_symlinked_root(), initialize(), Init, pairing_attempts_share_the_authentication_rate_limit(), refuses_to_export_server_state(), F, Option (+9 more)

### Community 66 - "Caching Design Rationale"
Cohesion: 0.13
Nodes (18): Adaptive Sequential Read-Ahead, Cache Writer Backpressure, Buffer Depth Is Not Fetch Concurrency, Cheap Content-Addressed Stores, Cold-Start Read-Ahead Priming, Persistent Write-Behind Group Commit, Process-Local Hot Range Tier, Multiple Sequential Streams Per Handle (+10 more)

### Community 67 - "GUI Pairing Code and Theme"
Cohesion: 0.12
Nodes (8): normalizePairingCode(), PAIRING_CODE_GROUPS, PAIRING_CODE_LENGTH, removeFormattingSeparators(), effectiveTheme, THEME_STORAGE_KEY, ThemePreference, target

### Community 68 - "Reconnect Policy Configuration"
Cohesion: 0.19
Nodes (13): AuthenticatedConnectionConfig, AuthenticatedConnector, ReconnectPolicy, RemoteFilesystemConnector, ResilientFilesystemOptions, Default, Duration, Self (+5 more)

### Community 69 - "FUSE Argument Iterator"
Cohesion: 0.24
Nodes (10): all_data(), ArgumentIterator, ArgumentIterator<'a>, generic_argument(), mixed_arguments(), out_of_data(), Option, OsStr (+2 more)

### Community 70 - "Read Request Flags"
Cohesion: 0.13
Nodes (5): ReadFlags, fuse_read_in, Read, ReadDir, ReadDirPlus

### Community 71 - "fuse2 Mount Implementation"
Cohesion: 0.18
Nodes (12): ensure_last_os_error(), MountImpl, Arc, CString, Error, Option, Path, Result (+4 more)

### Community 72 - "Directory Invalidation Protocol"
Cohesion: 0.17
Nodes (15): Node-Wide Directory Snapshot Invalidation, Enriched Revision-Validated Directory Projection, Directory Opens Without FOPEN_CACHE_DIR, Frame Compression (minor 6.3), Per-Connection and Global Known-Node Limits, Metadata Batching (minor 6.4), Server Node Registry, Self-Healing Node Eviction (+7 more)

### Community 73 - "Request Header Accessors"
Cohesion: 0.16
Nodes (9): Gid, Pid, TryFrom, AnyRequest<'a>, Uid, SystemTime, test_time_from_system_time_i64_min_boundary(), test_time_from_system_time_negative() (+1 more)

### Community 74 - "Rename and Link Requests"
Cohesion: 0.16
Nodes (5): Exchange<'a>, FilenameInDir, Link<'a>, Rename2<'a>, Rename<'a>

### Community 75 - "Client Directory View Chunking"
Cohesion: 0.19
Nodes (12): accept_directory_view_chunk(), ClientError, CreatedFile, DelayedFilesystem, directory_view_chunk_acceptable(), Arc, Duration, String (+4 more)

### Community 76 - "macFUSE Install Detection"
Cohesion: 0.24
Nodes (12): macfuse_bundle_is_installed(), macfuse_is_installed(), MacFuseRequired, recognizes_a_complete_filesystem_bundle(), rejects_missing_or_incomplete_bundles(), require_macfuse(), Box, Display (+4 more)

### Community 77 - "Authentication and Trust Design"
Cohesion: 0.20
Nodes (14): Argon2id Password Storage, Login and Pairing Rate Limits, Pairing-Assisted TOFU, Server Trust Separated From User Login, Operating-System / Public PKI Trust Mode, Trust-Policy TLS Preflight, Crate Change Boundaries, Export-Root Confinement (+6 more)

### Community 78 - "File Lock Requests"
Cohesion: 0.19
Nodes (6): fuse_file_lock, fuse_lk_in, fuse_lk_out, GetLk, Lock, SetLk

### Community 79 - "Reconnect Failure State"
Cohesion: 0.19
Nodes (8): invalid_handle(), ReconnectFailure, ReconnectState, AtomicBool, AtomicU64, ClientError, Instant, ScriptedFilesystem

### Community 80 - "Server Attribute Application"
Cohesion: 0.26
Nodes (11): apply_file_changes(), host_xattr_name(), read_fd_backup_time(), Option, SystemTime, rustix_timestamp(), system_time_from_millis(), system_time_millis() (+3 more)

### Community 81 - "Workspace Crate Manifests"
Cohesion: 0.32
Nodes (13): fuser, quickfs-auth, quickfs-cache, quickfs-client-cli, quickfs-client-core, quickfs-client-gui, quickfs-common, quickfs-filesystem-macfuse (+5 more)

### Community 82 - "Auth Identity Lock"
Cohesion: 0.27
Nodes (8): IdentityLock, Drop, File, Into, PathBuf, Self, StatePaths, UserDatabaseLock

### Community 83 - "Common Path Validation"
Cohesion: 0.24
Nodes (8): init_logging(), normalize_relative(), Path, PathBuf, Result, validate_filename(), validate_range(), ValidationError

### Community 84 - "Trust State and Certificates"
Cohesion: 0.20
Nodes (12): Client Trust Database, Enterprise Private CA Trust Mode, identity install Generation Rotation, Managed Exact Certificate Pin, Server Identity State Directory, Capability-Gated Optional Callbacks, Enterprise Setup Without Per-Client Pairing, Split of Server, Client, and Export State (+4 more)

### Community 85 - "fuse3 Mount Implementation"
Cohesion: 0.26
Nodes (9): ensure_last_os_error(), MountImpl, Arc, c_void, CString, Error, Path, Result (+1 more)

### Community 86 - "GUI Shell and Export Confinement"
Cohesion: 0.18
Nodes (11): Descriptor-Relative Export Root Confinement, client-gui HTML App Shell, theme-init.js Appearance Bootstrap, client-gui Tauri 2 + Svelte Desktop Client, macFUSE Runtime Preflight Check, Authenticated Connection Pipeline, Pairing-Assisted Exact Pin Bootstrap, Server Trust Modes (+3 more)

### Community 87 - "Aligned Data Wrappers"
Cohesion: 0.22
Nodes (8): Deref, DerefMut, NonZeroI32, Target, AlignedData, AlignedData<T>, ioslice_to_vec(), Vec

### Community 88 - "Security Audit Findings"
Cohesion: 0.27
Nodes (10): Dependabot Cargo and Actions Updates, Weekly RustSec Audit Job, Minor 6.4 GetMetadataBatch, Threat-Model-Driven Hardening of Remotely Reachable Paths, Hostile Server Hanging Streamed Directory-View Reassembly, Unknown-Node Full-Export Walk on the Async Runtime, One Connection Occupying Every Global Request Slot, Security Review of Remotely Reachable Paths (6.4) (+2 more)

### Community 89 - "Repo Contribution Templates"
Cohesion: 0.22
Nodes (10): Bug Report Issue Form, Blank Issues Disabled, Feature Request Issue Form, Pull Request Template, Minor 6.1 Directory Pagination, Minor 6.3 Zstd Frame Compression, Major.Minor Protocol Version Negotiation, Contributing Guide (+2 more)

### Community 90 - "Cache Tiering Backlog"
Cohesion: 0.22
Nodes (10): Persistent Cache Tiering and Invalidation, Collapse-Era Daemon Data-Integrity Fallout, SequentialPrefetcher in cached.rs, Single-Flight range_fetches Gate, Persistent Read-Through Cache, Revision-Aware Caching, Outstanding Engineering Backlog, Unresolved QUIC Connection Wedge (+2 more)

### Community 91 - "Poll Flags"
Cohesion: 0.22
Nodes (6): PollFlags, Display, Formatter, Result, fuse_poll_in, Poll

### Community 92 - "Finder Mount Race Investigation"
Cohesion: 0.25
Nodes (9): Native Directory Projection and Single-Flight Cold Load, Mount-Churn Test Methodology Lesson, Finder "Loading…"/Disconnect Investigation, fuser macFUSE INIT Capability Dialect Mismatch, Mount-Time Root Prewarm and statfs Cache, coreservicesd Volume-Registration Race, VOLUME_REGISTRATION_ATTEMPTS Raised to 8, ListDirectoryView Enriched Directory Projection (+1 more)

### Community 93 - "WAN Read-Ahead Investigation"
Cohesion: 0.25
Nodes (9): Adaptive Self-Tuning Read-Ahead Window, WAN BDP Measurement (82 ms / 50 Mbps RAID Link), High-Latency Read Performance Handoff, No Read-Ahead Across Kernel Reads, Cold First-Open Read-Ahead Priming, 32 MiB Speculative Prefetch Convoy Cap, Video Playback Stutter Fix (Multi-Stream Prefetcher, noreadahead), Protocol Version 6 (quickfs/6) (+1 more)

### Community 94 - "Batch Forget Requests"
Cohesion: 0.28
Nodes (4): ForgetOne, Self, fuse_forget_one, BatchForget<'a>

### Community 95 - "Poll Event Replies"
Cohesion: 0.25
Nodes (5): ResponseStruct<abi::fuse_poll_out>, PollEvents, Display, Formatter, Result

### Community 96 - "Platform Ports and CI Jobs"
Cohesion: 0.25
Nodes (8): Linux CI Job, macOS CI Job, Sync-Callback / Async-Runtime Bridge, macOS Client (client-cli and filesystem-macfuse), Unmount Watchdog and Forced Local Detach, Cross-Client Cache Coherence, Safe Out-of-Band Kernel Cache Invalidation, Server-Initiated Message Channel

### Community 97 - "Node Identity and Reconnect Invariants"
Cohesion: 0.25
Nodes (8): Self-Healing Per-Connection Node Working Set, Ambiguous Mutation Reporting, Opaque Stable Node Identity, Explicit Reconnect Invariants, Independent Protocol/Security Review Release Gate, Inode Reuse Aliasing of Node IDs, Reconnect Lock Replay Silently Discarded Held Locks, Unresolved Security Risks

### Community 98 - "Tauri Capability Permissions"
Cohesion: 0.25
Nodes (7): description, identifier, permissions, $schema, windows, core:default, main

### Community 99 - "GUI Tauri Backend Commands"
Cohesion: 0.32
Nodes (7): frontend_bootstrap(), FrontendBootstrap, main(), open_macfuse_install_page(), Result, String, run()

### Community 100 - "Test Support Fixtures"
Cohesion: 0.29
Nodes (7): deterministic_bytes(), fixture_export(), Path, Result, TempDir, Vec, write_fixture()

### Community 101 - "fuser Build and Quality Gates"
Cohesion: 0.29
Nodes (8): Local Quality Gate (scripts/check.sh), macfuse Feature Gate, Rust 1.88 / Edition 2024 Prerequisite, fuser CI Workflow, fuser macos-no-mount Compile-Only Mode, fuser Changelog, fuser MIT License, fuser (FUSE for Rust)

### Community 104 - "Getxattr Requests"
Cohesion: 0.33
Nodes (4): NonZeroU32, GetXAttr<'a>, GetXAttrSize, GetXAttrSizeEnum

### Community 106 - "Crate and Platform Boundaries"
Cohesion: 0.53
Nodes (6): Crate and Trust Boundaries, Linux Client (Planned), Windows Client via WinFsp (Planned), quicKFS Authenticated Network Filesystem, RemoteFilesystem Platform-Independent Interface, Platform Ports Excluded From This Backlog

### Community 107 - "Offline Write Policy"
Cohesion: 0.40
Nodes (6): Fail-Closed Offline Writes, No Cold-Start Offline Mount, quicKFS Roadmap Priorities, Server-Pushed Cross-Client Cache Invalidation (planned), WinFsp Support (planned), Windows Server (not planned)

### Community 108 - "CLI Usage Documentation"
Cohesion: 0.33
Nodes (6): quickfs-client-cli, quicKFS Usage and Command Reference, quickfs-server-daemon serve, Two-Gate Write Authorization, Linux Server Deployment, macOS Server (not implemented)

### Community 109 - "libfuse Build Script"
Cohesion: 0.60
Nodes (5): configure_libfuse2(), configure_libfuse3(), main(), Error, Result

### Community 110 - "Directory Entry Replies"
Cohesion: 0.53
Nodes (4): FileType, DirEntOffset, DirEntry, DirEntry<T>

### Community 111 - "Cuse Init Flags"
Cohesion: 0.40
Nodes (4): CuseInitFlags, Display, Formatter, Result

### Community 112 - "Fsync Flags"
Cohesion: 0.40
Nodes (4): FsyncFlags, Display, Formatter, Result

### Community 113 - "FUSE Read Buffer"
Cohesion: 0.33
Nodes (3): FuseReadBuf, Self, Vec

### Community 114 - "Postcard Versioning Rules"
Cohesion: 0.40
Nodes (5): Lossless Unix Byte Names, Guideline: Adding a Protocol Version, Metadata Wire Record, Known Non-Additive Changes, Postcard Positional Serialization

### Community 115 - "Getattr Flags"
Cohesion: 0.50
Nodes (3): GetattrFlags, fuse_getattr_in, GetAttr

### Community 116 - "Poll Client Example"
Cohesion: 0.60
Nodes (4): main(), make_nonblock(), AsFd, Result

### Community 117 - "pjdfs Test Harness"
Cohesion: 0.40
Nodes (3): PJDFS_EXIT_STATUS, RUST_BACKTRACE, pjdfs.sh script

### Community 118 - "Access Flags"
Cohesion: 0.40
Nodes (3): Display, Formatter, Result

### Community 124 - "BSD pjdfs Harness"
Cohesion: 0.40
Nodes (3): PJDFS_EXIT_STATUS, RUST_BACKTRACE, bsd_pjdfs.sh script

### Community 125 - "macOS pjdfs Harness"
Cohesion: 0.40
Nodes (3): PJDFS_EXIT_STATUS, RUST_BACKTRACE, macos_pjdfs.sh script

### Community 126 - "xfstests Harness"
Cohesion: 0.40
Nodes (3): RUST_BACKTRACE, xfstests.sh script, XFSTESTS_EXIT_STATUS

### Community 127 - "fuser macOS CI Workflows"
Cohesion: 0.67
Nodes (4): fuser FreeBSD Cirrus CI Task, macfuse-ci Composite Action, MacStadium PR CI Workflow, MacStadium Push CI Workflow

## Knowledge Gaps
- **151 isolated node(s):** `name`, `private`, `version`, `type`, `dev` (+146 more)
  These have ≤1 connection - possible missing edges or undocumented components.
- **22 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `INodeNo` connect `macFUSE Adapter Callbacks` to `fuser simple.rs Example FS`, `fuser Example Scaffolding`, `Low-Level Reply Buffers`, `fuser Notify and Poll Examples`, `FUSE Request ABI Structs`, `Request Header Accessors`, `Rename and Link Requests`, `Directory Entry Replies`, `fuser Passthrough Example`, `Kernel Cache Notifications`, `fuser Reply Senders`, `fuser Async Experimental API`, `Low-Level Notify Encoding`, `Kernel Capability Config`, `fuser Request Parsing`, `fuser Reply Types`, `Batch Forget Requests`?**
  _High betweenness centrality (0.193) - this node is a cross-community bridge._
- **Why does `T` connect `Protocol Frame Codec` to `QUIC Transport Layer`, `Server Export Session`, `Low-Level Reply Buffers`, `Auth Users and Pairing`, `NetworkFilesystem Client`, `fuser Async Experimental API`, `fuser Reply Types`, `Resilient Filesystem Wrapper`, `macFUSE Async Execution Bridge`, `macFUSE Adapter State`, `Cache Tier Containers`, `Protocol Encoding Tests`, `macFUSE Mount Lifecycle`, `Deferred Cache Commits`, `Resilient Write and Lock Ops`, `fuser Channel and Dispatch`, `Low-Level Notify Encoding`, `Mount and Unmount Syscalls`, `FUSE Argument Iterator`, `Client Directory View Chunking`, `Aligned Data Wrappers`, `Directory Entry Replies`?**
  _High betweenness centrality (0.176) - this node is a cross-community bridge._
- **Why does `Metadata` connect `Server Directory and Metadata Ops` to `Client CLI and Trust Commands`, `CachedFilesystem Implementation`, `Server Export Session`, `Auth Users and Pairing`, `macFUSE Adapter Callbacks`, `Client Cache Invalidation`, `macFUSE Flag Conversion`, `NetworkFilesystem Client`, `Cache Namespace and Validation`, `Server Core Integration Tests`, `macFUSE Mock Filesystem Tests`, `macFUSE Inode and Directory Cache`, `Resilient Filesystem Wrapper`, `macFUSE Async Execution Bridge`, `Export Discovery and Listing`, `macFUSE Adapter State`, `Hot Range Cache Tier`, `Cache Tier Containers`, `Protocol Encoding Tests`, `Deferred Cache Commits`, `macFUSE Async Mutations`, `Resilient Write and Lock Ops`, `Directory View Wire Types`, `Client Directory View Chunking`, `Server Attribute Application`?**
  _High betweenness centrality (0.122) - this node is a cross-community bridge._
- **Are the 2 inferred relationships involving `NodeId` (e.g. with `client_metadata_batch_negotiates_and_maps_results()` and `metadata_batch_resolves_per_node_and_bounds_size()`) actually correct?**
  _`NodeId` has 2 INFERRED edges - model-reasoned connections that need verification._
- **What connects `name`, `private`, `version` to the rest of the system?**
  _151 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `Client CLI and Trust Commands` be split into smaller, more focused modules?**
  _Cohesion score 0.056429463171036205 - nodes in this community are weakly interconnected._
- **Should `QUIC Transport Layer` be split into smaller, more focused modules?**
  _Cohesion score 0.06818181818181818 - nodes in this community are weakly interconnected._