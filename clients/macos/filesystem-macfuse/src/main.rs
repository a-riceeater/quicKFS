// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

/// `eprintln!` that never panics. Once the process that launched the mount
/// exits, stderr is a broken pipe and `eprintln!` would panic, which in the
/// signal task would abandon the unmount and leave the volume mounted.
macro_rules! note {
    ($($argument:tt)*) => {{
        use std::io::Write as _;
        let _ = writeln!(std::io::stderr(), $($argument)*);
    }};
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("quickfs-mount is available only on macOS")
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    // `QUICKFS_FUSE_DEBUG=1` streams every FUSE request the kernel dispatches
    // (and each error reply) to stderr via the `log` records `fuser` already
    // emits. This is the only visibility into a mount the kernel is talking
    // to, so keep it available in release binaries for field diagnosis.
    struct StderrLogger {
        started: std::time::Instant,
    }
    impl log::Log for StderrLogger {
        fn enabled(&self, metadata: &log::Metadata) -> bool {
            metadata.target().starts_with("fuser")
        }
        fn log(&self, record: &log::Record) {
            if !self.enabled(record.metadata()) {
                return;
            }
            let elapsed = self.started.elapsed();
            note!(
                "[{:>10.3} {} {}] {}",
                elapsed.as_secs_f64() * 1000.0,
                record.level(),
                record.target(),
                record.args()
            );
        }
        fn flush(&self) {}
    }
    if std::env::var_os("QUICKFS_FUSE_DEBUG").is_some() {
        let logger = Box::leak(Box::new(StderrLogger {
            started: std::time::Instant::now(),
        }));
        let _ = log::set_logger(logger);
        log::set_max_level(log::LevelFilter::Debug);
    }
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, bail};
    use clap::{Parser, ValueEnum};
    use quickfs_cache::{CacheNamespace, NonBlockingPersistentCache};
    use quickfs_client_core::{
        AuthenticatedConnectionConfig, CachePolicy, CachedFilesystem, MAX_CLIENT_READ_SIZE,
        ReconnectPolicy, RemoteFilesystem, ResilientFilesystem, ResilientFilesystemOptions,
        ServerTrust, load_trusted_server_pin,
    };
    use quickfs_filesystem_macfuse::{Adapter, MacFuseBackend, MountConfig, mount};
    use quickfs_transport_quic::load_certificates;
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::runtime::Builder;
    use zeroize::Zeroizing;

    // Let the resilient layer deliver its typed timeout (especially
    // AmbiguousMutation) before the callback bridge's last-resort guard fires.
    // This is an internal scheduling margin, not a second timeout setting.
    const DEADLINE_ERROR_DELIVERY_GRACE: Duration = Duration::from_millis(100);

    #[derive(Clone, Copy, Debug, Default, ValueEnum)]
    enum Backend {
        /// Use macFUSE's default backend (compatible with macFUSE 4).
        #[default]
        Auto,
        /// Use the macFUSE 5 FSKit backend on supported macOS releases.
        #[value(name = "fskit")]
        FsKit,
    }

    impl From<Backend> for MacFuseBackend {
        fn from(value: Backend) -> Self {
            match value {
                Backend::Auto => Self::Automatic,
                Backend::FsKit => Self::FsKit,
            }
        }
    }

    #[derive(Parser)]
    #[command(
        name = "quickfs-mount",
        about = "Mount an authenticated quicKFS export as a macFUSE volume"
    )]
    struct Cli {
        /// Existing directory on which the remote export will be mounted.
        mountpoint: PathBuf,
        #[arg(long, env = "QUICKFS_SERVER", default_value = "127.0.0.1:4433")]
        server: SocketAddr,
        #[arg(long, default_value = "localhost")]
        server_name: String,
        #[arg(
            long,
            env = "QUICKFS_CLIENT_STATE_DIR",
            default_value = ".quickfs-client"
        )]
        state_dir: PathBuf,
        #[arg(long, env = "QUICKFS_USERNAME")]
        username: Option<String>,
        /// Private directory for the persistent offline read cache.
        #[arg(long, env = "QUICKFS_CACHE_DIR")]
        cache_dir: Option<PathBuf>,
        /// Maximum cached payload bytes (metadata overhead is additional).
        #[arg(
            long,
            env = "QUICKFS_CACHE_MAX_BYTES",
            default_value_t = 20 * 1024 * 1024 * 1024_u64
        )]
        cache_max_bytes: u64,
        /// Read-ahead block size for overlapping and unaligned random reads.
        /// This is also the speculative sequential read-ahead fetch granularity;
        /// a smaller block trades more requests for finer pipelining on a fat,
        /// high-latency link.
        #[arg(long, default_value_t = 16_384)]
        cache_block_kib: u64,
        /// Ceiling on in-flight speculative read-ahead bytes for the whole
        /// mount. The adaptive prefetch window self-tunes below this to each
        /// link's bandwidth-delay product; raise it on a fat high-latency link,
        /// set 0 to disable speculative read-ahead. Kept under the server's
        /// in-flight read budget by default.
        #[arg(
            long,
            env = "QUICKFS_READ_AHEAD_MAX_BYTES",
            default_value_t = 64 * 1024 * 1024_u64
        )]
        read_ahead_max_bytes: u64,
        /// Maximum idle wait for one transport phase. Cold RAID directory
        /// scans and 16 MiB reads can legitimately exceed ten seconds.
        #[arg(long, default_value_t = 60_000)]
        timeout_ms: u64,
        /// Hard end-to-end budget for one resilient operation and its callback.
        #[arg(long, default_value_t = 120_000)]
        callback_timeout_ms: u64,
        #[arg(long, default_value_t = 3)]
        reconnect_attempts: usize,
        #[arg(long, default_value_t = 100)]
        reconnect_initial_backoff_ms: u64,
        #[arg(long, default_value_t = 2_000)]
        reconnect_max_backoff_ms: u64,
        /// Validate the server with the operating system's public/managed roots.
        #[arg(long, env = "QUICKFS_TRUST_SYSTEM_ROOTS", conflicts_with = "ca_cert")]
        trust_system_roots: bool,
        /// Validate the server with this PEM enterprise-CA bundle.
        #[arg(
            long,
            env = "QUICKFS_CA_CERT",
            value_name = "PEM",
            conflicts_with = "trust_system_roots"
        )]
        ca_cert: Option<PathBuf>,
        /// Volume label shown by Finder. Defaults to the export's renamed
        /// label if it has one, otherwise the mountpoint folder's name.
        #[arg(long, value_parser = parse_mount_name)]
        volume_name: Option<String>,
        /// Select the macFUSE 5 FSKit backend when the kernel extension is unavailable.
        #[arg(long, value_enum, default_value_t)]
        macfuse_backend: Backend,
        /// Read the password from the first line of standard input instead of
        /// prompting on the controlling terminal. The macOS app launches the
        /// mount without a terminal and supplies the Keychain credential here.
        #[arg(long)]
        password_stdin: bool,
        /// Rewrite this file every second with a JSON snapshot of the mount's
        /// QUIC link (round-trip time, congestion window, byte and packet
        /// counters). The macOS app reads it to chart a mount's throughput.
        #[arg(long, value_name = "PATH")]
        stats_file: Option<PathBuf>,
        /// Answer each `stats` line on standard input with one
        /// `quickfs-stats <json>` line on standard output holding the same
        /// snapshot `--stats-file` writes. Nothing is written to disk and no
        /// work is done between requests. The macOS app uses this; it keeps
        /// standard input open after the password line.
        #[arg(long)]
        stats_on_request: bool,
        /// Unmount when the launching process exits. The macOS app passes
        /// this so a crash or forced quit cannot leave an orphaned mount
        /// holding the cache and mountpoint.
        #[arg(long)]
        exit_with_parent: bool,
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        quickfs_macos_support::require_macfuse()?;
        if cli.timeout_ms == 0 || cli.callback_timeout_ms == 0 {
            bail!("transport and callback timeouts must be greater than zero");
        }
        if cli.cache_max_bytes == 0
            || cli.cache_block_kib == 0
            || cli.reconnect_attempts == 0
            || cli.reconnect_initial_backoff_ms > cli.reconnect_max_backoff_ms
        {
            bail!(
                "cache sizes and reconnect attempts must be positive, and reconnect backoff must not decrease"
            );
        }
        let mountpoint = validate_mountpoint(&cli.mountpoint)?;
        let username = cli
            .username
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--username or QUICKFS_USERNAME is required"))?;
        let trust = server_trust(&cli)?;
        let transport_timeout = Duration::from_millis(cli.timeout_ms);
        let operation_deadline = Duration::from_millis(cli.callback_timeout_ms);
        let runtime = Arc::new(
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("quickfs-remote")
                .build()
                .context("failed to create the shared Tokio runtime")?,
        );

        let identity_check = runtime
            .block_on(trust.connect(cli.server, &cli.server_name, transport_timeout))
            .with_context(|| {
                format!("failed to authenticate server via {}", trust.description())
            })?;
        let server_identity = identity_check
            .peer_certificate_fingerprint()
            .context("failed to read the authenticated server certificate identity")?;
        identity_check.close();

        let password = if cli.password_stdin {
            read_password_line(std::io::stdin().lock())?
        } else {
            Zeroizing::new(rpassword::prompt_password("Password: ")?)
        };
        // Reconnect after the potentially long prompt and reapply exactly the
        // same trust policy before transmitting the credential.
        let connection = AuthenticatedConnectionConfig::new(
            cli.server,
            cli.server_name.clone(),
            trust,
            username.clone(),
            password.to_string(),
            transport_timeout,
        );
        let resilient = runtime
            .block_on(ResilientFilesystem::connect_authenticated(
                connection,
                ResilientFilesystemOptions {
                    reconnect: ReconnectPolicy {
                        attempts: cli.reconnect_attempts,
                        initial_backoff: Duration::from_millis(cli.reconnect_initial_backoff_ms),
                        maximum_backoff: Duration::from_millis(cli.reconnect_max_backoff_ms),
                    },
                    operation_deadline,
                },
            ))
            .context("username/password authentication failed")?;
        let resilient: Arc<dyn RemoteFilesystem> = Arc::new(resilient);
        if let Some(path) = cli.stats_file.clone() {
            spawn_stats_writer(&runtime, Arc::clone(&resilient), path);
        }
        if cli.stats_on_request {
            spawn_stats_responder(Arc::clone(&resilient));
        }
        if cli.exit_with_parent {
            spawn_parent_watch(&runtime);
        }
        let capabilities = runtime
            .block_on(resilient.capabilities())
            .context("failed to query filesystem capabilities")?;

        let cache_namespace = CacheNamespace::new(
            server_identity,
            capabilities.server_epoch.to_string(),
            username,
        )
        .context("failed to construct the private cache namespace")?;
        let cache_root = cli
            .cache_dir
            .clone()
            .unwrap_or_else(|| cli.state_dir.clone());
        let cache = Arc::new(
            NonBlockingPersistentCache::open(&cache_root, cache_namespace, cli.cache_max_bytes)
                .with_context(|| {
                    format!(
                        "failed to open persistent cache at '{}'",
                        cache_root.display()
                    )
                })?,
        );
        let cache_block_size = cli
            .cache_block_kib
            .checked_mul(1024)
            .context("cache block size is too large")?
            .min(capabilities.max_read_size)
            .min(MAX_CLIENT_READ_SIZE);
        let filesystem = CachedFilesystem::new(
            resilient,
            cache,
            CachePolicy {
                block_size: cache_block_size,
                read_ahead_max_bytes: cli.read_ahead_max_bytes,
            },
        )
        .context("failed to configure the filesystem cache")?;
        let remote: Arc<dyn RemoteFilesystem> = Arc::new(filesystem);
        let adapter = Adapter::with_runtime(
            remote,
            operation_deadline.saturating_add(DEADLINE_ERROR_DELIVERY_GRACE),
            runtime,
        );
        let mount_config = MountConfig {
            volume_name: cli.volume_name,
            filesystem_name: "quickfs".into(),
            backend: cli.macfuse_backend.into(),
        };

        let access = if capabilities.writable {
            "read/write"
        } else {
            "read-only"
        };
        // stdout may also be a closed pipe; never let the banner panic.
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stdout(),
            "Preparing {access} {} ({}) for {} with reconnect and offline read caching. Press Control+C or run `umount '{}'` to unmount.",
            cli.server_name,
            cli.server,
            mountpoint.display(),
            mountpoint.display()
        );
        mount(adapter, &mountpoint, &mount_config).context("macFUSE mount failed")
    }

    fn server_trust(cli: &Cli) -> Result<ServerTrust> {
        if cli.trust_system_roots {
            return Ok(ServerTrust::system_roots());
        }
        if let Some(path) = &cli.ca_cert {
            let authorities = load_certificates(path).with_context(|| {
                format!("failed to load enterprise CA bundle '{}'", path.display())
            })?;
            return Ok(ServerTrust::enterprise_ca(authorities));
        }
        let fingerprint = load_trusted_server_pin(&cli.state_dir, cli.server, &cli.server_name)
            .context(
                "no exact pin is configured; pair or import a managed pin with quickfs-client-cli, \
             or use --ca-cert/--trust-system-roots for centrally managed PKI",
            )?;
        Ok(ServerTrust::pinned(fingerprint))
    }

    /// Start the normal graceful unmount (the SIGINT path) once the parent
    /// process is gone, which the kernel signals by reparenting us.
    fn spawn_parent_watch(runtime: &tokio::runtime::Runtime) {
        let parent = rustix::process::getppid();
        runtime.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(500));
            loop {
                ticker.tick().await;
                if rustix::process::getppid() != parent {
                    note!("the launching process exited; unmounting");
                    let _ = rustix::process::kill_process(
                        rustix::process::getpid(),
                        rustix::process::Signal::INT,
                    );
                    return;
                }
            }
        });
    }

    /// Publish link statistics for the app once a second. Each snapshot is
    /// written to a private temporary file and renamed into place, so a
    /// reader never sees a partial document. Failures are ignored: telemetry
    /// must never disturb the mount.
    fn spawn_stats_writer(
        runtime: &tokio::runtime::Runtime,
        filesystem: Arc<dyn RemoteFilesystem>,
        path: PathBuf,
    ) {
        let started = unix_millis();
        runtime.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let snapshot = stats_snapshot(started, filesystem.link_stats());
                let _ = write_private_atomically(&path, snapshot.to_string().as_bytes());
            }
        });
    }

    fn unix_millis() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or(0)
    }

    fn stats_snapshot(
        started_ms: u128,
        link: Option<quickfs_client_core::LinkStats>,
    ) -> serde_json::Value {
        serde_json::json!({
            "pid": std::process::id(),
            "started_ms": started_ms,
            "sampled_ms": unix_millis(),
            "link": link,
        })
    }

    /// Prefix of a `--stats-on-request` reply line, so the launcher can tell
    /// replies apart from the banner and log lines sharing standard output.
    const STATS_REPLY_PREFIX: &str = "quickfs-stats ";

    /// Longest request line read whole; longer lines are discarded, so a
    /// misbehaving parent cannot make the helper buffer unbounded input.
    const MAX_REQUEST_LINE_BYTES: u64 = 256;

    /// Serve `--stats-on-request` on a plain thread: standard input blocks,
    /// and `link_stats` never waits, so neither touches the FUSE runtime.
    fn spawn_stats_responder(filesystem: Arc<dyn RemoteFilesystem>) {
        let started = unix_millis();
        let spawned = std::thread::Builder::new()
            .name("quickfs-stats".into())
            .spawn(move || {
                serve_stats_requests(std::io::stdin().lock(), std::io::stdout(), || {
                    stats_snapshot(started, filesystem.link_stats())
                });
            });
        if let Err(error) = spawned {
            note!("link statistics are unavailable: {error}");
        }
    }

    /// Reply to each `stats` request line until input ends or output fails.
    /// Each reply is one `write_all` of a whole line, so it never interleaves
    /// with the other writers of standard output.
    fn serve_stats_requests(
        mut input: impl std::io::BufRead,
        mut output: impl std::io::Write,
        snapshot: impl Fn() -> serde_json::Value,
    ) {
        use std::io::{BufRead as _, Read as _};

        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut input)
                .take(MAX_REQUEST_LINE_BYTES)
                .read_until(b'\n', &mut line);
            match read {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.last() != Some(&b'\n') {
                // Over-long line: drop the rest of it unread into memory.
                if skip_line(&mut input).is_err() {
                    return;
                }
                continue;
            }
            if line.trim_ascii() != b"stats" {
                continue;
            }
            let reply = format!("{STATS_REPLY_PREFIX}{}\n", snapshot());
            if output
                .write_all(reply.as_bytes())
                .and_then(|()| output.flush())
                .is_err()
            {
                return;
            }
        }
    }

    fn skip_line(input: &mut impl std::io::BufRead) -> std::io::Result<()> {
        loop {
            let buffer = input.fill_buf()?;
            if buffer.is_empty() {
                return Ok(());
            }
            if let Some(end) = buffer.iter().position(|&byte| byte == b'\n') {
                input.consume(end + 1);
                return Ok(());
            }
            let length = buffer.len();
            input.consume(length);
        }
    }

    fn write_private_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(bytes)?;
        drop(file);
        std::fs::rename(&temporary, path)
    }

    /// Longest accepted stdin password line, so a misbehaving parent cannot
    /// make the mount buffer unbounded input before authenticating.
    const MAX_STDIN_PASSWORD_BYTES: u64 = 4_096;

    fn read_password_line(input: impl std::io::BufRead) -> Result<Zeroizing<String>> {
        use std::io::BufRead;

        let mut line = Zeroizing::new(Vec::new());
        input
            .take(MAX_STDIN_PASSWORD_BYTES + 1)
            .read_until(b'\n', &mut line)
            .context("failed to read the password from standard input")?;
        if u64::try_from(line.len()).unwrap_or(u64::MAX) > MAX_STDIN_PASSWORD_BYTES {
            bail!("the standard-input password exceeds {MAX_STDIN_PASSWORD_BYTES} bytes");
        }
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        if line.is_empty() {
            bail!("no password was supplied on standard input");
        }
        let text =
            std::str::from_utf8(&line).context("the standard-input password is not valid UTF-8")?;
        Ok(Zeroizing::new(text.to_owned()))
    }

    fn validate_mountpoint(path: &Path) -> Result<PathBuf> {
        let canonical = path
            .canonicalize()
            .with_context(|| format!("mountpoint '{}' does not exist", path.display()))?;
        if !canonical.is_dir() {
            bail!("mountpoint '{}' is not a directory", canonical.display());
        }
        Ok(canonical)
    }

    fn parse_mount_name(value: &str) -> std::result::Result<String, String> {
        if value.is_empty() || value.len() > 255 || value.contains(',') || value.contains('\0') {
            Err("mount name must contain 1-255 characters and no commas".into())
        } else {
            Ok(value.to_owned())
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::{read_password_line, serve_stats_requests};

        #[test]
        fn stdin_password_strips_line_endings_and_rejects_empty_or_huge_input() {
            assert_eq!(
                read_password_line(&b"secret\r\nignored"[..])
                    .unwrap()
                    .as_str(),
                "secret"
            );
            assert!(read_password_line(&b"\n"[..]).is_err());
            assert!(read_password_line(&vec![b'a'; 5_000][..]).is_err());
        }

        #[test]
        fn stats_requests_get_one_tagged_line_each_and_other_input_is_ignored() {
            let mut input = b"stats\nhello\n".to_vec();
            input.extend(vec![b'x'; 1_000]);
            input.extend(b"stats\n \tstats\r\nstats");
            let mut output = Vec::new();
            let calls = std::cell::Cell::new(0);
            serve_stats_requests(&input[..], &mut output, || {
                calls.set(calls.get() + 1);
                serde_json::json!({ "n": calls.get() })
            });
            // The over-long line swallows the "stats" glued to its end; the
            // final unterminated request is incomplete and not answered.
            assert_eq!(
                String::from_utf8(output).unwrap(),
                "quickfs-stats {\"n\":1}\nquickfs-stats {\"n\":2}\n"
            );
        }
    }
}
