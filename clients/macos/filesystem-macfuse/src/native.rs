// SPDX-License-Identifier: Apache-2.0

use super::{Adapter, AdapterError, CreatedNode, DirectoryListing};
use fuser::{
    BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, ForgetOne, Generation, INodeNo, InitFlags, IoctlFlags, KernelConfig, LockOwner,
    MountOption, OpenAccMode, OpenFlags, PollEvents, PollFlags, PollNotifier, RenameFlags,
    ReplyAttr, ReplyBmap, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty,
    ReplyEntry, ReplyIoctl, ReplyLock, ReplyLseek, ReplyOpen, ReplyPoll, ReplyStatfs, ReplyWrite,
    ReplyXTimes, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use quickfs_client_core::ClientError;
use quickfs_protocol::{
    AttributeChanges, ErrorCode, FileAccess, FileLock, FileOpenOptions, LockKind, Metadata, Name,
    NodeKind, RenameMode, SafeIoctl, SeekWhence, SpecialNodeKind, XattrSetMode,
};
use std::{
    ffi::OsStr,
    future::Future,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::runtime::Runtime;

const ATTRIBUTE_TTL: Duration = Duration::from_secs(1);
const BLOCK_SIZE: u32 = 4_096;
const MAX_BACKGROUND_REQUESTS: u16 = 64;
const CONGESTION_THRESHOLD: u16 = 48;
const GRACEFUL_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(3);
const FORCE_UNMOUNT_TIMEOUT: Duration = Duration::from_secs(5);
const UNMOUNT_WATCHDOG_TIMEOUT: Duration = Duration::from_secs(8);
/// How long after mount(2) to wait before probing whether macOS registered
/// the volume usably; the registration probes arrive within the first
/// couple of seconds.
const VOLUME_REGISTRATION_SETTLE: Duration = Duration::from_secs(5);
/// Mount attempts before giving up on a usable volume registration and
/// serving the mount with a warning. A broken registration is permanent for
/// that mount, so each retry is a fresh mount(2).
const VOLUME_REGISTRATION_ATTEMPTS: u32 = 4;
/// Pause between registration retries so the previous volume's CoreServices
/// teardown finishes before the replacement mount registers.
const VOLUME_REGISTRATION_RETRY_DELAY: Duration = Duration::from_secs(10);
const DARWIN_IOCTL_INOUT: u32 = 0xc000_0000;

const fn darwin_iowr(group: u8, number: u8, size: u32) -> u32 {
    DARWIN_IOCTL_INOUT | ((size & 0x1fff) << 16) | ((group as u32) << 8) | number as u32
}

// Darwin implements lseek(..., SEEK_HOLE/SEEK_DATA) by issuing these private
// vnode ioctls. macFUSE forwards them as FUSE_IOCTL rather than FUSE_LSEEK.
const FSIOC_FIOSEEKHOLE: u32 = darwin_iowr(b'A', 16, std::mem::size_of::<i64>() as u32);
const FSIOC_FIOSEEKDATA: u32 = darwin_iowr(b'A', 17, std::mem::size_of::<i64>() as u32);

#[derive(Clone, Debug)]
pub struct MountConfig {
    pub volume_name: String,
    pub filesystem_name: String,
    pub backend: MacFuseBackend,
}

/// macFUSE transport used to bridge the userspace FUSE protocol into macOS.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MacFuseBackend {
    /// Let macFUSE select its default backend. This remains compatible with
    /// macFUSE 4 and normally uses the kernel extension.
    #[default]
    Automatic,
    /// Use macFUSE 5's FSKit backend, avoiding the legacy kernel extension on
    /// supported macOS releases.
    FsKit,
}

impl Default for MountConfig {
    fn default() -> Self {
        Self {
            volume_name: "quicKFS".into(),
            filesystem_name: "quickfs".into(),
            backend: MacFuseBackend::Automatic,
        }
    }
}

pub fn mount(adapter: Adapter, mountpoint: &Path, config: &MountConfig) -> io::Result<()> {
    validate_mount_config(config)?;
    let capabilities = adapter
        .probe_capabilities()
        .map_err(|error| io::Error::other(error.to_string()))?;
    let volume_name = if config.volume_name == MountConfig::default().volume_name {
        capabilities.volume_name.clone()
    } else {
        config.volume_name.clone()
    };
    let mut fuser_config = Config::default();
    fuser_config.mount_options.extend([
        if capabilities.writable {
            MountOption::RW
        } else {
            MountOption::RO
        },
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoExec,
        MountOption::NoAtime,
        MountOption::FSName(config.filesystem_name.clone()),
        // No MountOption::Subtype here: `subtype=` is a Linux mtab concept.
        // macFUSE's mount helper does not define it, and passing options the
        // helper does not understand is exactly the class of divergence that
        // broke LaunchServices/Finder on this volume (see the macFUSE INIT
        // dialect note in vendor/fuser).
        MountOption::CUSTOM(format!("volname={volume_name}")),
        // Store extended attributes in AppleDouble (`._name`) sidecars that
        // macFUSE manages, instead of forwarding every xattr natively. This is
        // required for `cp`/Finder to copy files carrying `com.apple.quarantine`
        // (anything downloaded from the internet). macOS propagates quarantine
        // through the Quarantine framework's `qtn_file_apply_to_fd`, which macFUSE
        // rejects with EINVAL in-kernel on a native-xattr volume without ever
        // dispatching a setxattr the filesystem could satisfy; `cp` then aborts
        // with "fcopyfile failed: Invalid argument". Under `auto_xattr` macFUSE
        // absorbs the quarantine into the sidecar and the copy succeeds.
        //
        // The trade-off is that `auto_xattr` makes macFUSE look up a `._name`
        // sidecar per file, ~doubling the inodes the kernel references during a
        // crawl. That is why the server's per-connection node ceiling is sized
        // generously (see docs/protocol.md and the daemon's
        // --max-known-nodes-per-connection flag): an undersized ceiling would
        // surface here as directory listings failing with "too many known nodes".
        MountOption::CUSTOM("auto_xattr".into()),
    ]);
    if !capabilities.supports_special_nodes {
        fuser_config.mount_options.push(MountOption::NoDev);
    }
    // macFUSE's kernel daemon timeout (default 60s) force-unmounts the volume
    // if any callback outruns it. The adapter's own callback deadline is longer
    // and replies with an error rather than dropping the reply, so give the
    // kernel a strictly larger budget; otherwise a slow cold RAID scan or read
    // is ejected mid-flight and the volume "disconnects".
    let daemon_timeout = macfuse_daemon_timeout_secs(adapter.callback_timeout());
    fuser_config.mount_options.push(MountOption::CUSTOM(format!(
        "daemon_timeout={daemon_timeout}"
    )));
    if let Some(option) = backend_mount_option(config.backend) {
        fuser_config.mount_options.push(option);
    }
    // fuser 0.17 deliberately rejects multiple receive loops on macOS. Every
    // potentially blocking callback below moves its reply into the shared
    // Tokio runtime instead, so the one receive loop remains responsive.
    fuser_config.n_threads = Some(1);
    let runtime = adapter.runtime().clone();

    // Mount, verify that macOS registered the volume usably, and retry when
    // it did not. CoreServices registers each new volume by probing it under
    // a short internal deadline; when the registration races wrong (observed
    // as a coin flip on a degraded host and near-certain on a high-latency
    // link before the pre-mount cache warmup existed), coreservicesd keeps a
    // broken file-ID tree for the volume and every Finder/LaunchServices
    // interaction fails with EIO for the life of the mount, even though
    // terminal I/O works. The state is per-mount and permanent, so the only
    // recovery is to unmount and mount again. The health probe is the same
    // call LaunchServices makes (FSPathMakeRef → ioErr on a broken volume).
    for attempt in 1..=VOLUME_REGISTRATION_ATTEMPTS {
        // Warm the root directory view, root metadata, and filesystem
        // statistics so the registration probes are answered from memory
        // rather than paying one network round trip each. This also keeps
        // Finder's first LOOKUP/READDIR against a cold RAID directory from
        // reading as an unresponsive volume.
        adapter.prewarm_for_mount().map_err(|error| {
            io::Error::other(format!("failed to prepare the root directory: {error}"))
        })?;
        let mut session = fuser::Session::new(adapter.clone(), mountpoint, &fuser_config)?;
        eprintln!("quicKFS mount is ready at {}", mountpoint.display());
        let health_unmounter = session.unmount_callable();
        let registration_broken = Arc::new(AtomicBool::new(false));
        let can_retry = attempt < VOLUME_REGISTRATION_ATTEMPTS;
        spawn_registration_health_check(
            mountpoint.to_path_buf(),
            health_unmounter,
            Arc::clone(&registration_broken),
            can_retry,
        );
        let result = run_session_until_unmount(session, mountpoint, &runtime)?;
        if registration_broken.load(Ordering::Acquire) && can_retry {
            eprintln!(
                "remounting {} (attempt {} of {}) after macOS registered the volume unusably",
                mountpoint.display(),
                attempt + 1,
                VOLUME_REGISTRATION_ATTEMPTS,
            );
            std::thread::sleep(VOLUME_REGISTRATION_RETRY_DELAY);
            continue;
        }
        match &result {
            Ok(()) => eprintln!("quicKFS mount session ended"),
            Err(error) => eprintln!("quicKFS mount session failed: {error}"),
        }
        return result;
    }
    unreachable!("the volume-registration retry loop always returns on its final attempt")
}

/// Run one mounted fuser session to completion with Ctrl-C/SIGTERM unmount
/// handling. The outer `Result` reports setup errors (signal registration);
/// the inner one is the session outcome itself.
#[allow(clippy::type_complexity)]
fn run_session_until_unmount(
    mut session: fuser::Session<Adapter>,
    mountpoint: &Path,
    runtime: &Arc<Runtime>,
) -> io::Result<io::Result<()>> {
    let mut unmounter = session.unmount_callable();
    let signal_mountpoint = mountpoint.to_path_buf();
    let (mut interrupt, mut terminate) = {
        let _runtime_guard = runtime.enter();
        (
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?,
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
        )
    };
    let unmount_complete = Arc::new(AtomicBool::new(false));
    let signal_unmount_complete = Arc::clone(&unmount_complete);
    let signal_task = runtime.spawn(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        eprintln!("Unmounting quicKFS...");
        let watchdog_mountpoint = signal_mountpoint.clone();
        let watchdog_unmount_complete = Arc::clone(&signal_unmount_complete);
        if let Err(error) = std::thread::Builder::new()
            .name("quickfs-unmount-watchdog".into())
            .spawn(move || {
                std::thread::sleep(UNMOUNT_WATCHDOG_TIMEOUT);
                if watchdog_unmount_complete.load(Ordering::Acquire) {
                    return;
                }
                eprintln!(
                    "unmount did not finish within {} seconds; closing the mount process to detach {}",
                    UNMOUNT_WATCHDOG_TIMEOUT.as_secs(),
                    watchdog_mountpoint.display()
                );
                std::process::exit(130);
            })
        {
            eprintln!("failed to start unmount watchdog: {error}; closing the mount process");
            std::process::exit(130);
        }
        let graceful = tokio::task::spawn_blocking(move || unmounter.unmount());
        let graceful = tokio::time::timeout(GRACEFUL_UNMOUNT_TIMEOUT, graceful).await;
        if !matches!(graceful, Ok(Ok(Ok(())))) {
            eprintln!("graceful unmount did not complete; forcing a local detach");
            if let Err(error) = force_local_unmount(signal_mountpoint).await {
                eprintln!("forced unmount failed: {error}; closing the mount process");
                std::process::exit(130);
            }
        }
    });
    let result = session.run();
    unmount_complete.store(true, Ordering::Release);
    signal_task.abort();
    Ok(result)
}

/// Whether macOS considers the mounted volume usable for Finder and
/// LaunchServices. AppleScript's legacy `as alias` coercion exercises the
/// same Carbon FSRef machinery LaunchServices uses for every document open
/// (`FSPathMakeRef`), and `osascript` ships with macOS. On a volume whose
/// coreservicesd registration is broken the coercion fails with error
/// `-1700` (a failed coercion wrapping the underlying `ioErr`); anything
/// else — success, an unexpected error, a timeout, or the volume already
/// being unmounted — is treated as healthy so an ambiguous probe can never
/// cause a remount loop.
fn volume_registration_is_broken(mountpoint: &Path) -> bool {
    let Some(path) = mountpoint.to_str() else {
        return false;
    };
    let probe = Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "POSIX file (item 1 of argv) as alias",
            "-e",
            "end run",
            "--",
            path,
        ])
        .output();
    match probe {
        Ok(output) if output.status.success() => false,
        Ok(output) => String::from_utf8_lossy(&output.stderr).contains("-1700"),
        Err(_) => false,
    }
}

/// Probe whether macOS registered the freshly mounted volume usably, from a
/// helper thread once the registration window has settled. A broken
/// registration is permanent for the life of the mount: when retrying is
/// still possible the volume is unmounted so the caller can mount again,
/// otherwise the mount is kept serving (terminal I/O still works) with a
/// loud warning.
fn spawn_registration_health_check(
    mountpoint: PathBuf,
    mut unmounter: fuser::SessionUnmounter,
    registration_broken: Arc<AtomicBool>,
    can_retry: bool,
) {
    let spawned = std::thread::Builder::new()
        .name("quickfs-registration-health".into())
        .spawn(move || {
            std::thread::sleep(VOLUME_REGISTRATION_SETTLE);
            if !volume_registration_is_broken(&mountpoint) {
                return;
            }
            registration_broken.store(true, Ordering::Release);
            if can_retry {
                eprintln!(
                    "macOS registered {} unusably for Finder (alias probe failed); remounting",
                    mountpoint.display()
                );
                if let Err(error) = unmounter.unmount() {
                    eprintln!("health-check unmount failed: {error}");
                }
            } else {
                eprintln!(
                    "warning: macOS registered {} unusably for Finder (alias probe failed) and \
                     the retry budget is exhausted; terminal access keeps working, but opening \
                     the volume in Finder will fail with an input/output error until it is \
                     remounted. A reboot clears degraded CoreServices state that makes this \
                     more likely.",
                    mountpoint.display()
                );
            }
        });
    if let Err(error) = spawned {
        eprintln!("failed to start the volume-registration health check: {error}");
    }
}

async fn force_local_unmount(mountpoint: PathBuf) -> io::Result<()> {
    let worker = tokio::task::spawn_blocking(move || {
        let status = Command::new("/sbin/umount")
            .arg("-f")
            .arg(&mountpoint)
            .status()?;
        if status.success() {
            return Ok(());
        }
        let status = Command::new("/usr/sbin/diskutil")
            .arg("unmount")
            .arg("force")
            .arg(&mountpoint)
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "local unmount commands exited with {status}"
            )))
        }
    });
    tokio::time::timeout(FORCE_UNMOUNT_TIMEOUT, worker)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "forced unmount timed out"))?
        .map_err(io::Error::other)?
}

/// macFUSE accepts a daemon timeout in whole seconds within roughly 0..=600.
/// Give the kernel a margin above the adapter's callback deadline so the
/// adapter always wins the race and replies with a real error code instead of
/// being force-unmounted mid-callback.
fn macfuse_daemon_timeout_secs(callback_timeout: Duration) -> u64 {
    const MACFUSE_MAX_DAEMON_TIMEOUT: u64 = 600;
    const MARGIN: u64 = 15;
    callback_timeout
        .as_secs()
        .saturating_add(MARGIN)
        .clamp(30, MACFUSE_MAX_DAEMON_TIMEOUT)
}

fn backend_mount_option(backend: MacFuseBackend) -> Option<MountOption> {
    match backend {
        MacFuseBackend::Automatic => None,
        MacFuseBackend::FsKit => Some(MountOption::CUSTOM("backend=fskit".into())),
    }
}

fn validate_mount_config(config: &MountConfig) -> io::Result<()> {
    for value in [&config.volume_name, &config.filesystem_name] {
        if value.is_empty() || value.len() > 255 || value.contains(',') || value.contains('\0') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "mount names must contain 1-255 characters and no commas",
            ));
        }
    }
    Ok(())
}

impl Adapter {
    fn spawn_callback(&self, future: impl Future<Output = ()> + Send + 'static) {
        let task = self.runtime().spawn(future);
        drop(task);
    }
}

impl Filesystem for Adapter {
    fn init(&mut self, _request: &Request, config: &mut KernelConfig) -> io::Result<()> {
        let Some(capabilities) = self.cached_capabilities() else {
            return Err(io::Error::other(
                "filesystem capabilities were not loaded before mount",
            ));
        };

        for capability in [
            InitFlags::FUSE_FILE_OPS,
            InitFlags::FUSE_AUTO_INVAL_DATA,
            InitFlags::FUSE_PARALLEL_DIROPS,
        ] {
            add_capability_if_supported(config, capability);
        }
        if capabilities.supports_preallocation {
            add_capability_if_supported(config, InitFlags::FUSE_ALLOCATE);
        }
        if capabilities.supports_exchange_data {
            add_capability_if_supported(config, InitFlags::FUSE_EXCHANGE_DATA);
        }
        if capabilities.supports_atomic_rename {
            // On macOS bits 25/26 are FUSE_CAP_RENAME_SWAP/RENAME_EXCL.
            // fuser 0.17 exposes those bits under their Linux names.
            add_capability_if_supported(config, InitFlags::FUSE_EXPLICIT_INVAL_DATA);
            add_capability_if_supported(config, InitFlags::FUSE_MAP_ALIGNMENT);
        }
        let _ = config.set_max_background(MAX_BACKGROUND_REQUESTS);
        let _ = config.set_congestion_threshold(CONGESTION_THRESHOLD);
        // One FUSE read may be larger than one negotiated wire read; the
        // adapter splits it while preserving the file revision.
        let maximum_readahead = super::MAX_FUSE_IO_SIZE.min(u64::from(u32::MAX));
        if let Ok(maximum_readahead) = u32::try_from(maximum_readahead) {
            let _ = config.set_max_readahead(maximum_readahead);
        }

        if capabilities.writable {
            add_capability_if_supported(config, InitFlags::FUSE_ATOMIC_O_TRUNC);
            add_capability_if_supported(config, InitFlags::FUSE_BIG_WRITES);
            // Keep one kernel write within one wire write so O_APPEND remains
            // atomic across clients. `write_async` still chunks larger direct
            // adapter calls defensively.
            let maximum = capabilities
                .max_write_size
                .min(super::MAX_FUSE_IO_SIZE)
                .min(u64::from(u32::MAX));
            if let Ok(maximum) = u32::try_from(maximum) {
                let _ = config.set_max_write(maximum);
            }
        }
        if capabilities.supports_locks {
            add_capability_if_supported(config, InitFlags::FUSE_POSIX_LOCKS);
        }
        if capabilities.supports_readdirplus {
            add_capability_if_supported(config, InitFlags::FUSE_DO_READDIRPLUS);
            add_capability_if_supported(config, InitFlags::FUSE_READDIRPLUS_AUTO);
        }
        Ok(())
    }

    fn destroy(&mut self) {
        self.destroy_mount();
    }

    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.lookup_async(u64::from(parent), name).await {
                Ok(result) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, result.inode, &result.metadata),
                    Generation(0),
                ),
                Err(error) => {
                    if !is_expected_lookup_miss(&error) {
                        report_callback_error("lookup", &error);
                    }
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn forget(&self, _request: &Request, inode: INodeNo, nlookup: u64) {
        // forget has no reply. Evicting an inode scans the handle and directory
        // maps, so run it off the single macFUSE receive-loop thread; otherwise
        // Finder's bursty forget/batch_forget traffic during a crawl stalls the
        // one reader and later requests never get dispatched.
        let adapter = self.clone();
        self.spawn_callback(async move {
            let _ = adapter.forget_inode(u64::from(inode), nlookup);
        });
    }

    fn batch_forget(&self, _request: &Request, nodes: &[ForgetOne]) {
        let requests = nodes
            .iter()
            .map(|node| (u64::from(node.nodeid()), node.nlookup()))
            .collect::<Vec<_>>();
        let adapter = self.clone();
        self.spawn_callback(async move {
            let _ = adapter.forget_inodes(&requests);
        });
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter.getattr_async(inode).await {
                Ok(metadata) => {
                    reply.attr(&ATTRIBUTE_TTL, &file_attr(&adapter, inode, &metadata));
                }
                Err(error) => {
                    report_callback_error("getattr", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        ctime: Option<SystemTime>,
        handle: Option<FileHandle>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        if uid.is_some()
            || gid.is_some()
            || ctime.is_some()
            || crtime.is_some()
            || chgtime.is_some()
            || flags.is_some()
        {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let accessed_unix_ms = match atime.map(time_or_now_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let modified_unix_ms = match mtime.map(time_or_now_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let backup_unix_ms = match bkuptime.map(system_time_millis).transpose() {
            Ok(value) => value,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter
                .setattr_async(
                    inode,
                    handle.map(u64::from),
                    AttributeChanges {
                        size,
                        mode: mode.map(|value| value & 0o7777),
                        accessed_unix_ms,
                        modified_unix_ms,
                        backup_unix_ms,
                    },
                )
                .await
            {
                Ok(metadata) => {
                    reply.attr(&ATTRIBUTE_TTL, &file_attr(&adapter, inode, &metadata));
                }
                Err(error) => {
                    report_callback_error("setattr", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn readlink(&self, _request: &Request, inode: INodeNo, reply: ReplyData) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.readlink_async(u64::from(inode)).await {
                Ok(target) => reply.data(&target),
                Err(error) => {
                    report_callback_error("readlink", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn mknod(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let permissions = permission_mode(mode, umask);
        let kind = match mode & u32::from(libc::S_IFMT) {
            value if value == u32::from(libc::S_IFREG) => None,
            value if value == u32::from(libc::S_IFIFO) => Some(SpecialNodeKind::NamedPipe),
            value if value == u32::from(libc::S_IFCHR) => Some(SpecialNodeKind::CharacterDevice),
            value if value == u32::from(libc::S_IFBLK) => Some(SpecialNodeKind::BlockDevice),
            value if value == u32::from(libc::S_IFSOCK) => Some(SpecialNodeKind::Socket),
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            let result = if let Some(kind) = kind {
                let device = rdev as libc::dev_t;
                adapter
                    .create_special_node_async(
                        u64::from(parent),
                        name,
                        kind,
                        permissions,
                        u32::try_from(libc::major(device)).unwrap_or_default(),
                        u32::try_from(libc::minor(device)).unwrap_or_default(),
                    )
                    .await
            } else {
                match adapter
                    .create_file_async(
                        u64::from(parent),
                        name,
                        permissions,
                        FileOpenOptions {
                            access: FileAccess::ReadWrite,
                            truncate: false,
                            append: false,
                        },
                    )
                    .await
                {
                    Ok(created) => {
                        let node = CreatedNode {
                            inode: created.inode,
                            metadata: created.metadata,
                        };
                        let _ = adapter.release_async(created.handle, false, None).await;
                        Ok(node)
                    }
                    Err(error) => Err(error),
                }
            };
            match result {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => {
                    report_callback_error("mknod", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_directory_async(u64::from(parent), name, permission_mode(mode, umask))
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn unlink(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        remove_callback(self, parent, name, false, reply);
    }

    fn rmdir(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        remove_callback(self, parent, name, true, reply);
    }

    fn symlink(
        &self,
        _request: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let link_name = Name::new(link_name.as_bytes().to_vec());
        let target = target.as_os_str().as_bytes().to_vec();
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_symlink_async(u64::from(parent), link_name, target)
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn link(
        &self,
        _request: &Request,
        inode: INodeNo,
        new_parent: INodeNo,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        let new_name = Name::new(new_name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_hard_link_async(u64::from(inode), u64::from(new_parent), new_name)
                .await
            {
                Ok(created) => reply.entry(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let mode = match rename_mode(flags) {
            Ok(mode) => mode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let name = Name::new(name.as_bytes().to_vec());
        let new_name = Name::new(new_name.as_bytes().to_vec());
        rename_callback(self, parent, name, new_parent, new_name, mode, reply);
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let options = open_options(flags);
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.open_async(u64::from(inode), options).await {
                Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let options = open_options(OpenFlags(flags));
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .create_file_async(
                    u64::from(parent),
                    name,
                    permission_mode(mode, umask),
                    options,
                )
                .await
            {
                Ok(created) => reply.created(
                    &ATTRIBUTE_TTL,
                    &file_attr(&adapter, created.inode, &created.metadata),
                    Generation(0),
                    FileHandle(created.handle),
                    FopenFlags::empty(),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn read(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .read_async(u64::from(handle), offset, u64::from(size))
                .await
            {
                Ok(data) => reply.data(&data),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let data = data.to_vec();
        let synchronize = write_sync_mode(flags);
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.write_async(u64::from(handle), offset, &data).await {
                Ok(written) => {
                    if let Some(data_only) = synchronize
                        && let Err(error) = adapter.fsync_async(u64::from(handle), data_only).await
                    {
                        reply.error(errno(&error));
                        return;
                    }
                    match u32::try_from(written) {
                        Ok(written) => reply.written(written),
                        Err(_) => reply.error(Errno::EOVERFLOW),
                    }
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn flush(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .flush_async(u64::from(handle), Some(lock_owner.0))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        lock_owner: Option<LockOwner>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .release_async(u64::from(handle), flush, lock_owner.map(|owner| owner.0))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn fsync(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        data_only: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.fsync_async(u64::from(handle), data_only).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn opendir(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EISDIR);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.opendir_async(u64::from(inode)).await {
                Ok(handle) => {
                    // The adapter retains one coherent enriched snapshot for
                    // Finder's follow-up callbacks. Do not additionally pin a
                    // kernel directory cache across create/rename/remove.
                    reply.opened(FileHandle(handle), FopenFlags::empty());
                }
                Err(error) => {
                    report_callback_error("opendir", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.directory_listing(u64::from(handle), u64::from(inode)) {
                Ok(listing) => {
                    fill_directory(
                        &adapter,
                        &mut reply,
                        u64::from(inode),
                        offset,
                        listing.as_ref(),
                    );

                    reply.ok();
                }
                Err(error) => {
                    report_callback_error("readdir", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn readdirplus(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            let inode = u64::from(inode);
            match adapter.directory_listing_with_metadata(u64::from(handle), inode) {
                Ok((listing, current, parent)) => {
                    fill_directory_plus(
                        &adapter,
                        &mut reply,
                        inode,
                        offset,
                        listing.as_ref(),
                        &current,
                        &parent,
                    );
                    reply.ok();
                }
                Err(error) => {
                    report_callback_error("readdirplus", &error);
                    reply.error(errno(&error));
                }
            }
        });
    }

    fn releasedir(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        match Adapter::releasedir(self, u64::from(handle)) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn fsyncdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .fsyncdir_async(u64::from(inode), u64::from(handle))
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn statfs(&self, _request: &Request, _inode: INodeNo, reply: ReplyStatfs) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.statfs_async().await {
                Ok(stats) => reply.statfs(
                    stats.blocks,
                    stats.blocks_free,
                    stats.blocks_available,
                    stats.files,
                    stats.files_free,
                    stats.block_size,
                    stats.name_length,
                    stats.fragment_size,
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        let mode = match xattr_set_mode(flags) {
            Ok(mode) => mode,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let name = Name::new(name.as_bytes().to_vec());
        let value = value.to_vec();
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .set_xattr_async(u64::from(inode), name, value, mode, position)
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn getxattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            if size == 0 {
                match adapter.xattr_size_async(u64::from(inode), name).await {
                    Ok(length) => match u32::try_from(length) {
                        Ok(length) => reply.size(length),
                        Err(_) => reply.error(Errno::E2BIG),
                    },
                    Err(error) => reply.error(errno(&error)),
                }
            } else {
                match adapter.get_xattr_async(u64::from(inode), name).await {
                    Ok(value) if value.len() <= size as usize => reply.data(&value),
                    Ok(_) => reply.error(Errno::ERANGE),
                    Err(error) => reply.error(errno(&error)),
                }
            }
        });
    }

    fn listxattr(&self, _request: &Request, inode: INodeNo, size: u32, reply: ReplyXattr) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.list_xattrs_async(u64::from(inode)).await {
                Ok(names) => {
                    let mut encoded = Vec::new();
                    for name in names {
                        encoded.extend_from_slice(name.as_bytes());
                        encoded.push(0);
                    }
                    if size == 0 {
                        match u32::try_from(encoded.len()) {
                            Ok(length) => reply.size(length),
                            Err(_) => reply.error(Errno::E2BIG),
                        }
                    } else if encoded.len() <= size as usize {
                        reply.data(&encoded);
                    } else {
                        reply.error(Errno::ERANGE);
                    }
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn removexattr(&self, _request: &Request, inode: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.remove_xattr_async(u64::from(inode), name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn getlk(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        reply: ReplyLock,
    ) {
        let lock = match protocol_lock(lock_owner, start, end, typ, pid) {
            Ok(lock) => lock,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.get_lock_async(u64::from(handle), lock).await {
                Ok(Some(conflict)) => reply.locked(
                    conflict.start,
                    conflict.end,
                    lock_type(conflict.kind),
                    conflict.pid,
                ),
                Ok(None) => reply.locked(0, 0, i32::from(libc::F_UNLCK), 0),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setlk(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        lock_owner: LockOwner,
        start: u64,
        end: u64,
        typ: i32,
        pid: u32,
        sleep: bool,
        reply: ReplyEmpty,
    ) {
        let lock = match protocol_lock(lock_owner, start, end, typ, pid) {
            Ok(lock) => lock,
            Err(error) => {
                reply.error(error);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.set_lock_async(u64::from(handle), lock, sleep).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn bmap(
        &self,
        _request: &Request,
        inode: INodeNo,
        block_size: u32,
        block: u64,
        reply: ReplyBmap,
    ) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .map_block_async(u64::from(inode), block_size, block)
                .await
            {
                Ok(mapped) => reply.bmap(mapped),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn ioctl(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        flags: IoctlFlags,
        command: u32,
        input: &[u8],
        output_size: u32,
        reply: ReplyIoctl,
    ) {
        if command == FSIOC_FIOSEEKHOLE || command == FSIOC_FIOSEEKDATA {
            if !flags.is_empty()
                || input.len() != std::mem::size_of::<i64>()
                || output_size < std::mem::size_of::<i64>() as u32
            {
                reply.error(Errno::ENOTTY);
                return;
            }
            let Ok(bytes) = <[u8; std::mem::size_of::<i64>()]>::try_from(input) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let offset = i64::from_ne_bytes(bytes);
            let Ok(offset) = u64::try_from(offset) else {
                reply.error(Errno::EINVAL);
                return;
            };
            let whence = if command == FSIOC_FIOSEEKDATA {
                SeekWhence::Data
            } else {
                SeekWhence::Hole
            };
            let adapter = self.clone();
            self.spawn_callback(async move {
                match adapter.lseek_async(u64::from(handle), offset, whence).await {
                    Ok(offset) => match i64::try_from(offset) {
                        Ok(offset) => reply.ioctl(0, &offset.to_ne_bytes()),
                        Err(_) => reply.error(Errno::EOVERFLOW),
                    },
                    Err(error) => reply.error(errno(&error)),
                }
            });
            return;
        }
        if !flags.is_empty()
            || !input.is_empty()
            || command != libc::FIONREAD as u32
            || output_size < std::mem::size_of::<i32>() as u32
        {
            reply.error(Errno::ENOTTY);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .safe_ioctl_async(u64::from(handle), SafeIoctl::BytesAvailable)
                .await
            {
                Ok(value) => {
                    let value = i32::try_from(value).unwrap_or(i32::MAX);
                    reply.ioctl(0, &value.to_ne_bytes());
                }
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn poll(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _notifier: PollNotifier,
        events: PollEvents,
        _flags: PollFlags,
        reply: ReplyPoll,
    ) {
        match self.poll_events(u64::from(handle), events) {
            Ok(ready) => reply.poll(ready),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn fallocate(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        if mode != 0 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .allocate_async(u64::from(handle), offset, length)
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn lseek(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let Ok(offset) = u64::try_from(offset) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let whence = match whence {
            libc::SEEK_DATA => SeekWhence::Data,
            libc::SEEK_HOLE => SeekWhence::Hole,
            _ => {
                reply.error(Errno::EINVAL);
                return;
            }
        };
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.lseek_async(u64::from(handle), offset, whence).await {
                Ok(offset) => match i64::try_from(offset) {
                    Ok(offset) => reply.offset(offset),
                    Err(_) => reply.error(Errno::EOVERFLOW),
                },
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _request: &Request,
        _input_inode: INodeNo,
        input_handle: FileHandle,
        input_offset: u64,
        _output_inode: INodeNo,
        output_handle: FileHandle,
        output_offset: u64,
        length: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .copy_file_range_async(
                    u64::from(input_handle),
                    input_offset,
                    u64::from(output_handle),
                    output_offset,
                    length,
                )
                .await
            {
                Ok(copied) => match u32::try_from(copied) {
                    Ok(copied) => reply.written(copied),
                    Err(_) => reply.error(Errno::EOVERFLOW),
                },
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn setvolname(&self, _request: &Request, name: &OsStr, reply: ReplyEmpty) {
        let name = Name::new(name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.set_volume_name_async(name).await {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn exchange(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        options: u64,
        reply: ReplyEmpty,
    ) {
        let name = Name::new(name.as_bytes().to_vec());
        let new_name = Name::new(new_name.as_bytes().to_vec());
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter
                .exchange_data_async(
                    u64::from(parent),
                    name,
                    u64::from(new_parent),
                    new_name,
                    options,
                )
                .await
            {
                Ok(()) => reply.ok(),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }

    fn getxtimes(&self, _request: &Request, inode: INodeNo, reply: ReplyXTimes) {
        let adapter = self.clone();
        self.spawn_callback(async move {
            match adapter.getattr_async(u64::from(inode)).await {
                Ok(metadata) => reply.xtimes(
                    metadata
                        .backup_unix_ms
                        .map(millis_time)
                        .unwrap_or(UNIX_EPOCH),
                    metadata
                        .created_unix_ms
                        .map(millis_time)
                        .unwrap_or_else(|| metadata_time(&metadata)),
                ),
                Err(error) => reply.error(errno(&error)),
            }
        });
    }
}

fn add_capability_if_supported(config: &mut KernelConfig, capability: InitFlags) {
    if config.capabilities().contains(capability) {
        let _ = config.add_capabilities(capability);
    }
}

fn remove_callback(
    adapter: &Adapter,
    parent: INodeNo,
    name: &OsStr,
    directory: bool,
    reply: ReplyEmpty,
) {
    let name = Name::new(name.as_bytes().to_vec());
    let owned = adapter.clone();
    adapter.spawn_callback(async move {
        match owned.remove_async(u64::from(parent), name, directory).await {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn rename_callback(
    adapter: &Adapter,
    parent: INodeNo,
    name: Name,
    new_parent: INodeNo,
    new_name: Name,
    mode: RenameMode,
    reply: ReplyEmpty,
) {
    let owned = adapter.clone();
    adapter.spawn_callback(async move {
        match owned
            .rename_async(
                u64::from(parent),
                name,
                u64::from(new_parent),
                new_name,
                mode,
            )
            .await
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    });
}

fn fill_directory(
    adapter: &Adapter,
    reply: &mut ReplyDirectory,
    inode: u64,
    requested_offset: u64,
    listing: &DirectoryListing,
) {
    let mut index = 0_u64;
    if add_directory_row(
        reply,
        requested_offset,
        &mut index,
        inode,
        FileType::Directory,
        OsStr::new("."),
    ) || add_directory_row(
        reply,
        requested_offset,
        &mut index,
        listing.parent_inode,
        FileType::Directory,
        OsStr::new(".."),
    ) {
        return;
    }
    for entry in &listing.entries {
        let entry_inode = adapter
            .remember_entry(entry.metadata.node, inode, &entry.name)
            .unwrap_or(entry.inode);
        if add_directory_row(
            reply,
            requested_offset,
            &mut index,
            entry_inode,
            file_type(entry.kind),
            OsStr::from_bytes(entry.name.as_bytes()),
        ) {
            break;
        }
    }
}

fn add_directory_row(
    reply: &mut ReplyDirectory,
    requested_offset: u64,
    index: &mut u64,
    inode: u64,
    kind: FileType,
    name: &OsStr,
) -> bool {
    let current = *index;
    *index = (*index).saturating_add(1);
    current >= requested_offset && reply.add(INodeNo(inode), *index, kind, name)
}

#[allow(clippy::too_many_arguments)]
fn fill_directory_plus(
    adapter: &Adapter,
    reply: &mut ReplyDirectoryPlus,
    inode: u64,
    requested_offset: u64,
    listing: &DirectoryListing,
    current: &Metadata,
    parent: &Metadata,
) {
    let mut index = 0_u64;
    let current_eligible = index >= requested_offset;
    if add_directory_plus_row(
        reply,
        requested_offset,
        &mut index,
        inode,
        OsStr::new("."),
        &file_attr(adapter, inode, current),
    ) {
        return;
    }
    if current_eligible {
        let _ = adapter.add_lookup(inode, 1);
    }
    let parent_eligible = index >= requested_offset;
    if add_directory_plus_row(
        reply,
        requested_offset,
        &mut index,
        listing.parent_inode,
        OsStr::new(".."),
        &file_attr(adapter, listing.parent_inode, parent),
    ) {
        return;
    }
    if parent_eligible {
        let _ = adapter.add_lookup(listing.parent_inode, 1);
    }
    for entry in &listing.entries {
        let eligible = index >= requested_offset;
        let entry_inode = adapter
            .remember_entry(entry.metadata.node, inode, &entry.name)
            .unwrap_or(entry.inode);
        let full = add_directory_plus_row(
            reply,
            requested_offset,
            &mut index,
            entry_inode,
            OsStr::from_bytes(entry.name.as_bytes()),
            &file_attr(adapter, entry_inode, &entry.metadata),
        );
        if full {
            break;
        }
        if eligible {
            let _ = adapter.add_lookup(entry_inode, 1);
        }
    }
}

fn add_directory_plus_row(
    reply: &mut ReplyDirectoryPlus,
    requested_offset: u64,
    index: &mut u64,
    inode: u64,
    name: &OsStr,
    attr: &FileAttr,
) -> bool {
    let current = *index;
    *index = index.saturating_add(1);
    current >= requested_offset
        && reply.add(
            INodeNo(inode),
            *index,
            name,
            &ATTRIBUTE_TTL,
            attr,
            Generation(0),
        )
}

fn open_options(flags: OpenFlags) -> FileOpenOptions {
    let access = match flags.acc_mode() {
        OpenAccMode::O_RDONLY => FileAccess::ReadOnly,
        OpenAccMode::O_WRONLY => FileAccess::WriteOnly,
        OpenAccMode::O_RDWR => FileAccess::ReadWrite,
    };
    FileOpenOptions {
        access,
        truncate: flags.0 & libc::O_TRUNC != 0,
        append: flags.0 & libc::O_APPEND != 0,
    }
}

fn rename_mode(flags: RenameFlags) -> Result<RenameMode, Errno> {
    match flags.bits() {
        0 => Ok(RenameMode::Replace),
        value if value == libc::RENAME_SWAP => Ok(RenameMode::Exchange),
        value if value == libc::RENAME_EXCL => Ok(RenameMode::NoReplace),
        _ => Err(Errno::EOPNOTSUPP),
    }
}

fn write_sync_mode(flags: OpenFlags) -> Option<bool> {
    if flags.0 & libc::O_SYNC != 0 {
        Some(false)
    } else if flags.0 & libc::O_DSYNC != 0 {
        Some(true)
    } else {
        None
    }
}

fn permission_mode(mode: u32, umask: u32) -> u32 {
    mode & !umask & 0o7777
}

fn time_or_now_millis(value: TimeOrNow) -> Result<u64, Errno> {
    let time = match value {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    let duration = time.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
    u64::try_from(duration.as_millis()).map_err(|_| Errno::EOVERFLOW)
}

fn system_time_millis(time: SystemTime) -> Result<u64, Errno> {
    let duration = time.duration_since(UNIX_EPOCH).map_err(|_| Errno::EINVAL)?;
    u64::try_from(duration.as_millis()).map_err(|_| Errno::EOVERFLOW)
}

fn xattr_set_mode(flags: i32) -> Result<XattrSetMode, Errno> {
    match flags {
        0 => Ok(XattrSetMode::Upsert),
        value if value == libc::XATTR_CREATE => Ok(XattrSetMode::Create),
        value if value == libc::XATTR_REPLACE => Ok(XattrSetMode::Replace),
        _ => Err(Errno::EINVAL),
    }
}

fn protocol_lock(
    owner: LockOwner,
    start: u64,
    end: u64,
    typ: i32,
    pid: u32,
) -> Result<FileLock, Errno> {
    let kind = match typ {
        value if value == i32::from(libc::F_RDLCK) => LockKind::Read,
        value if value == i32::from(libc::F_WRLCK) => LockKind::Write,
        value if value == i32::from(libc::F_UNLCK) => LockKind::Unlock,
        _ => return Err(Errno::EINVAL),
    };
    if end < start {
        return Err(Errno::EINVAL);
    }
    Ok(FileLock {
        owner: owner.0,
        start,
        end,
        kind,
        pid,
    })
}

fn lock_type(kind: LockKind) -> i32 {
    match kind {
        LockKind::Read => i32::from(libc::F_RDLCK),
        LockKind::Write => i32::from(libc::F_WRLCK),
        LockKind::Unlock => i32::from(libc::F_UNLCK),
    }
}

fn file_attr(adapter: &Adapter, inode: u64, metadata: &Metadata) -> FileAttr {
    let kind = file_type(metadata.kind);
    let permission = u16::try_from(metadata.mode & 0o7777).unwrap_or_default();
    let accessed = millis_time(metadata.accessed_unix_ms);
    let modified = metadata_time(metadata);
    let created = metadata
        .created_unix_ms
        .map(millis_time)
        .unwrap_or(UNIX_EPOCH);
    FileAttr {
        ino: INodeNo(inode),
        size: metadata.size,
        blocks: metadata.allocated_blocks,
        atime: accessed,
        mtime: modified,
        // The protocol does not yet carry a distinct inode-change timestamp.
        ctime: modified,
        crtime: created,
        kind,
        perm: permission,
        nlink: metadata.link_count,
        uid: adapter.owner_uid(),
        gid: adapter.owner_gid(),
        rdev: libc::makedev(metadata.device_major as _, metadata.device_minor as _) as u32,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn metadata_time(metadata: &Metadata) -> SystemTime {
    millis_time(metadata.modified_unix_ms)
}

fn millis_time(milliseconds: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(milliseconds))
        .unwrap_or(UNIX_EPOCH)
}

fn file_type(kind: NodeKind) -> FileType {
    match kind {
        NodeKind::File => FileType::RegularFile,
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
        NodeKind::NamedPipe => FileType::NamedPipe,
        NodeKind::CharacterDevice => FileType::CharDevice,
        NodeKind::BlockDevice => FileType::BlockDevice,
        NodeKind::Socket => FileType::Socket,
    }
}

fn errno(error: &AdapterError) -> Errno {
    match error {
        AdapterError::CallbackTimedOut => Errno::ETIMEDOUT,
        AdapterError::UnknownInode => Errno::ESTALE,
        AdapterError::UnknownHandle | AdapterError::UnknownDirectoryHandle => Errno::EBADF,
        AdapterError::HandleInodeMismatch => Errno::EBADF,
        AdapterError::NotFound => Errno::ENOENT,
        AdapterError::InvalidName | AdapterError::InvalidRange => Errno::EINVAL,
        AdapterError::InvalidRemoteName
        | AdapterError::AmbiguousName
        | AdapterError::Runtime(_)
        | AdapterError::StateUnavailable
        | AdapterError::InvalidCapabilities => Errno::EIO,
        AdapterError::InodeSpaceExhausted => Errno::ENFILE,
        AdapterError::HandleSpaceExhausted => Errno::EMFILE,
        AdapterError::UnexpectedMetadata
        | AdapterError::UnexpectedReadLength
        | AdapterError::UnexpectedWriteLength => Errno::EPROTO,
        AdapterError::StaleRevision => Errno::ESTALE,
        AdapterError::RequestTooLarge(_) => Errno::EFBIG,
        AdapterError::ReadOnly => Errno::EROFS,
        AdapterError::Unsupported => Errno::EOPNOTSUPP,
        AdapterError::InvalidAccess => Errno::EBADF,
        AdapterError::Client(client) => client_errno(client),
    }
}

fn report_callback_error(operation: &str, error: &AdapterError) {
    eprintln!("quicKFS {operation} failed: {error}");
}

fn is_expected_lookup_miss(error: &AdapterError) -> bool {
    matches!(
        error,
        AdapterError::NotFound | AdapterError::Client(ClientError::Server(ErrorCode::NotFound, _))
    )
}

fn client_errno(error: &ClientError) -> Errno {
    match error {
        ClientError::Transport(_) => Errno::EIO,
        ClientError::UnexpectedResponse => Errno::EPROTO,
        ClientError::ReadTooLarge(_) | ClientError::WriteTooLarge(_) => Errno::EFBIG,
        ClientError::StaleRevision => Errno::ESTALE,
        ClientError::Offline | ClientError::OfflineCacheMiss => Errno::ENETDOWN,
        ClientError::AmbiguousMutation => Errno::EIO,
        ClientError::Server(code, _) => match code {
            ErrorCode::Unauthenticated | ErrorCode::PermissionDenied => Errno::EACCES,
            ErrorCode::NotFound => Errno::ENOENT,
            ErrorCode::AlreadyExists => Errno::EEXIST,
            ErrorCode::NotDirectory => Errno::ENOTDIR,
            ErrorCode::IsDirectory => Errno::EISDIR,
            ErrorCode::NotEmpty => Errno::ENOTEMPTY,
            ErrorCode::NoAttribute => Errno::NO_XATTR,
            ErrorCode::NoData => Errno::ENXIO,
            ErrorCode::NotTty => Errno::ENOTTY,
            ErrorCode::ReadOnly => Errno::EROFS,
            ErrorCode::Conflict => Errno::ESTALE,
            ErrorCode::WouldBlock => Errno::EAGAIN,
            ErrorCode::NoSpace => Errno::ENOSPC,
            ErrorCode::Busy => Errno::EBUSY,
            ErrorCode::NotSupported => Errno::EOPNOTSUPP,
            ErrorCode::Offline => Errno::ENETDOWN,
            ErrorCode::InvalidNode => Errno::ESTALE,
            ErrorCode::InvalidHandle => Errno::EBADF,
            ErrorCode::InvalidRequest => Errno::EINVAL,
            ErrorCode::UnsupportedVersion => Errno::EPROTO,
            ErrorCode::TooLarge => Errno::EFBIG,
            ErrorCode::Timeout => Errno::ETIMEDOUT,
            ErrorCode::Internal => Errno::EIO,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_macfuse_fskit_backend_without_changing_the_default() {
        assert_eq!(MountConfig::default().backend, MacFuseBackend::Automatic);
        assert!(backend_mount_option(MacFuseBackend::Automatic).is_none());
        assert!(matches!(
            backend_mount_option(MacFuseBackend::FsKit),
            Some(MountOption::CUSTOM(option)) if option == "backend=fskit"
        ));
    }

    #[test]
    fn converts_open_flags_without_losing_append_or_truncate() {
        let options = open_options(OpenFlags(libc::O_RDWR | libc::O_APPEND | libc::O_TRUNC));
        assert_eq!(options.access, FileAccess::ReadWrite);
        assert!(options.append);
        assert!(options.truncate);
    }

    #[test]
    fn converts_posix_lock_types_and_rejects_invalid_ranges() {
        let lock = protocol_lock(LockOwner(8), 10, 20, i32::from(libc::F_WRLCK), 42).unwrap();
        assert_eq!(lock.owner, 8);
        assert_eq!(lock.kind, LockKind::Write);
        assert!(protocol_lock(LockOwner(8), 20, 10, i32::from(libc::F_WRLCK), 42).is_err());
        assert!(protocol_lock(LockOwner(8), 0, 1, -99, 42).is_err());
    }

    #[test]
    fn applies_umask_and_strips_file_type_bits() {
        assert_eq!(
            permission_mode(u32::from(libc::S_IFREG | 0o666), 0o027),
            0o640
        );
    }

    #[test]
    fn maps_synchronous_write_flags_to_full_or_data_only_sync() {
        assert_eq!(write_sync_mode(OpenFlags(libc::O_SYNC)), Some(false));
        assert_eq!(write_sync_mode(OpenFlags(libc::O_DSYNC)), Some(true));
        assert_eq!(write_sync_mode(OpenFlags(libc::O_RDWR)), None);
    }

    #[test]
    fn maps_macos_renamex_flags_without_confusing_data_exchange() {
        assert_eq!(
            rename_mode(RenameFlags::empty()).unwrap(),
            RenameMode::Replace
        );
        assert_eq!(
            rename_mode(RenameFlags::from_bits_retain(libc::RENAME_SWAP)).unwrap(),
            RenameMode::Exchange
        );
        assert_eq!(
            rename_mode(RenameFlags::from_bits_retain(libc::RENAME_EXCL)).unwrap(),
            RenameMode::NoReplace
        );
        assert!(rename_mode(RenameFlags::from_bits_retain(u32::MAX)).is_err());
    }

    #[test]
    fn encodes_darwin_sparse_seek_ioctls_with_an_off_t_payload() {
        assert_eq!(FSIOC_FIOSEEKHOLE, 0xc008_4110);
        assert_eq!(FSIOC_FIOSEEKDATA, 0xc008_4111);
    }
}
