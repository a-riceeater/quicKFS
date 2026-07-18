// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("quickfs-mount is available only on macOS")
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{Context, Result, bail};
    use clap::{Parser, ValueEnum};
    use quickfs_cache::{CacheNamespace, PersistentCache};
    use quickfs_client_core::{
        AuthenticatedConnectionConfig, CachePolicy, CachedFilesystem, ReconnectPolicy,
        RemoteFilesystem, ResilientFilesystem, ServerTrust, load_trusted_server_pin,
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
        #[arg(long, default_value_t = 1_024)]
        cache_block_kib: u64,
        #[arg(long, default_value_t = 10_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 45_000)]
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
        #[arg(long, default_value = "quicKFS", value_parser = parse_mount_name)]
        volume_name: String,
        /// Select the macFUSE 5 FSKit backend when the kernel extension is unavailable.
        #[arg(long, value_enum, default_value_t)]
        macfuse_backend: Backend,
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

        let password = Zeroizing::new(rpassword::prompt_password("Password: ")?);
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
                ReconnectPolicy {
                    attempts: cli.reconnect_attempts,
                    initial_backoff: Duration::from_millis(cli.reconnect_initial_backoff_ms),
                    maximum_backoff: Duration::from_millis(cli.reconnect_max_backoff_ms),
                },
            ))
            .context("username/password authentication failed")?;
        let resilient: Arc<dyn RemoteFilesystem> = Arc::new(resilient);
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
            PersistentCache::open(&cache_root, cache_namespace, cli.cache_max_bytes).with_context(
                || {
                    format!(
                        "failed to open persistent cache at '{}'",
                        cache_root.display()
                    )
                },
            )?,
        );
        let cache_block_size = cli
            .cache_block_kib
            .checked_mul(1024)
            .context("cache block size is too large")?;
        let filesystem = CachedFilesystem::new(
            resilient,
            cache,
            CachePolicy {
                block_size: cache_block_size,
            },
        )
        .context("failed to configure the filesystem cache")?;
        let remote: Arc<dyn RemoteFilesystem> = Arc::new(filesystem);
        let adapter = Adapter::with_runtime(
            remote,
            Duration::from_millis(cli.callback_timeout_ms),
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
        println!(
            "Mounting {access} {} ({}) at {} with reconnect and offline read caching. Unmount with `diskutil unmount '{}'`.",
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
}
