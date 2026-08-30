// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! `quickfs` — the Windows client command line.
//!
//! One binary covers the whole lifecycle of a Windows mount: establishing trust
//! in a server, mounting an export on a drive letter, and taking it down again.
//!
//! ```text
//! quickfs pair --pairing-id <uuid>        # authenticate and pin a server
//! quickfs mount 192.0.2.10:4433 Q:        # mount it, staying in the foreground
//! quickfs unmount Q:                      # from another window
//! ```
//!
//! The flags mirror `quickfs-mount` and `quickfs-client-cli` on macOS, and the
//! trust database, the offline cache and the server-validation policy are the
//! same code on both platforms. Only the two subcommands that place a volume in
//! a namespace differ, because the namespaces do.

#[cfg(not(windows))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("quickfs is available only on Windows; use quickfs-mount on macOS")
}

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    windows::run()
}

#[cfg(windows)]
mod windows {
    use anyhow::{Context, Result, bail};
    use clap::{Parser, Subcommand};
    use quickfs_cache::{CacheNamespace, NonBlockingPersistentCache};
    use quickfs_client_core::{
        AuthenticatedConnectionConfig, CachePolicy, CachedFilesystem, MAX_CLIENT_READ_SIZE,
        ReconnectPolicy, RemoteFilesystem, ResilientFilesystem, ResilientFilesystemOptions,
        ServerTrust, TrustStore, load_trusted_server_pin, parse_sha256_fingerprint,
    };
    use quickfs_filesystem_winfsp::{MountConfig, MountPoint, mount};
    use quickfs_transport_quic::{certificate_sha256, load_certificates};
    use quickfs_windows_support::{UnmountSignal, require_winfsp, signal_unmount};
    use std::{
        net::SocketAddr,
        path::PathBuf,
        sync::{Arc, mpsc},
        time::Duration,
    };
    use tokio::runtime::Builder;
    use uuid::Uuid;
    use zeroize::{Zeroize, Zeroizing};

    /// Let the resilient layer deliver its typed timeout — especially
    /// `AmbiguousMutation` — before the callback bridge's last-resort guard
    /// fires. An internal scheduling margin, not a second timeout setting.
    const DEADLINE_ERROR_DELIVERY_GRACE: Duration = Duration::from_millis(100);

    #[derive(Parser)]
    #[command(
        name = "quickfs",
        about = "Mount an authenticated quicKFS export as a Windows volume",
        version
    )]
    struct Cli {
        #[arg(
            long,
            global = true,
            env = "QUICKFS_CLIENT_STATE_DIR",
            default_value = ".quickfs-client"
        )]
        state_dir: PathBuf,
        #[arg(long, global = true, default_value = "localhost")]
        server_name: String,
        #[arg(long, global = true, default_value_t = 30_000)]
        timeout_ms: u64,
        /// Validate the server with the operating system's public/managed roots.
        #[arg(
            long,
            global = true,
            env = "QUICKFS_TRUST_SYSTEM_ROOTS",
            conflicts_with = "ca_cert"
        )]
        trust_system_roots: bool,
        /// Validate the server with this PEM enterprise-CA bundle.
        #[arg(
            long,
            global = true,
            env = "QUICKFS_CA_CERT",
            value_name = "PEM",
            conflicts_with = "trust_system_roots"
        )]
        ca_cert: Option<PathBuf>,
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Mount an export and stay in the foreground until it is unmounted.
        Mount {
            /// Server address, as `host:port`.
            #[arg(env = "QUICKFS_SERVER")]
            server: SocketAddr,
            /// Drive letter (`Q:`), an existing empty directory, or `*` for the
            /// next free letter.
            mountpoint: String,
            #[arg(long, env = "QUICKFS_USERNAME")]
            username: Option<String>,
            /// Private directory for the persistent offline read cache.
            #[arg(long, env = "QUICKFS_CACHE_DIR")]
            cache_dir: Option<PathBuf>,
            /// Maximum cached payload bytes (metadata overhead is additional).
            #[arg(long, env = "QUICKFS_CACHE_MAX_BYTES", default_value_t = 20 * 1024 * 1024 * 1024_u64)]
            cache_max_bytes: u64,
            /// Read-ahead block size for overlapping and unaligned random
            /// reads, and the speculative sequential fetch granularity.
            #[arg(long, default_value_t = 16_384)]
            cache_block_kib: u64,
            /// Ceiling on in-flight speculative read-ahead bytes for the whole
            /// mount. Set 0 to disable speculative read-ahead.
            #[arg(long, env = "QUICKFS_READ_AHEAD_MAX_BYTES", default_value_t = 64 * 1024 * 1024_u64)]
            read_ahead_max_bytes: u64,
            /// Maximum idle wait for one transport phase.
            #[arg(long, default_value_t = 60_000)]
            transport_timeout_ms: u64,
            /// Hard end-to-end budget for one operation and its callback.
            #[arg(long, default_value_t = 120_000)]
            callback_timeout_ms: u64,
            #[arg(long, default_value_t = 3)]
            reconnect_attempts: usize,
            #[arg(long, default_value_t = 100)]
            reconnect_initial_backoff_ms: u64,
            #[arg(long, default_value_t = 2_000)]
            reconnect_max_backoff_ms: u64,
            /// Volume label shown in Explorer.
            #[arg(long, default_value = "quicKFS", value_parser = parse_volume_name)]
            volume_name: String,
        },
        /// Ask the process holding a mountpoint to unmount it.
        Unmount {
            /// Drive letter (`Q:`) or directory the volume is mounted on.
            mountpoint: String,
        },
        /// Pair with a server and pin its identity.
        Pair {
            #[arg(env = "QUICKFS_SERVER")]
            server: SocketAddr,
            #[arg(long)]
            pairing_id: Uuid,
            #[arg(long, env = "QUICKFS_PAIRING_CODE")]
            code: Option<String>,
        },
        /// Remove a pinned identity so deliberate re-pairing can occur.
        Forget {
            #[arg(env = "QUICKFS_SERVER")]
            server: SocketAddr,
        },
        /// Manage exact certificate pins without interactive pairing.
        Trust {
            #[command(subcommand)]
            command: TrustCommand,
        },
    }

    #[derive(Subcommand)]
    enum TrustCommand {
        /// Import a centrally distributed exact certificate fingerprint.
        Import {
            #[arg(env = "QUICKFS_SERVER")]
            server: SocketAddr,
            /// SHA-256 fingerprint as 64 hexadecimal digits (colons accepted).
            #[arg(
                long,
                required_unless_present = "certificate",
                conflicts_with = "certificate"
            )]
            sha256: Option<String>,
            /// PEM certificate or chain whose first certificate will be pinned.
            #[arg(
                long,
                value_name = "PEM",
                required_unless_present = "sha256",
                conflicts_with = "sha256"
            )]
            certificate: Option<PathBuf>,
        },
        /// Show every pinned server identity.
        List,
    }

    impl Drop for Command {
        fn drop(&mut self) {
            // A pairing code passed on the command line is a bearer secret for
            // the length of one pairing; clear it rather than leaving it in the
            // process image.
            if let Self::Pair {
                code: Some(code), ..
            } = self
            {
                code.zeroize();
            }
        }
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        let timeout = Duration::from_millis(cli.timeout_ms);
        match &cli.command {
            Command::Mount { .. } => {
                // Only mounting needs the WinFsp runtime. Pairing and trust
                // management are ordinary file and network operations, and
                // refusing them before WinFsp is installed would force an
                // awkward ordering on a first-time setup.
                require_winfsp()?;
                run_mount(&cli)
            }
            Command::Unmount { mountpoint } => run_unmount(mountpoint),
            Command::Pair {
                server,
                pairing_id,
                code,
            } => {
                reject_enterprise_flags(&cli)?;
                run_pair(&cli, *server, *pairing_id, code.clone(), timeout)
            }
            Command::Forget { server } => {
                reject_enterprise_flags(&cli)?;
                run_forget(&cli, *server)
            }
            Command::Trust { command } => {
                reject_enterprise_flags(&cli)?;
                run_trust(&cli, command)
            }
        }
    }

    /// The enterprise-PKI switches select how an *authenticated* connection
    /// validates a server. Accepting them on a pairing or trust-store command
    /// would imply they affect what gets pinned, which they do not.
    fn reject_enterprise_flags(cli: &Cli) -> Result<()> {
        if cli.trust_system_roots || cli.ca_cert.is_some() {
            bail!("--trust-system-roots and --ca-cert apply only to mounting");
        }
        Ok(())
    }

    fn run_pair(
        cli: &Cli,
        server: SocketAddr,
        pairing_id: Uuid,
        code: Option<String>,
        timeout: Duration,
    ) -> Result<()> {
        let store = TrustStore::open(&cli.state_dir)?;
        store.ensure_unpinned(server, &cli.server_name)?;
        let code = Zeroizing::new(match code {
            Some(code) => code,
            None => rpassword::prompt_password("Pairing code: ")?,
        });
        let guard = store.lock()?;
        // The prompt can be arbitrarily long, so the writer lock is taken only
        // afterwards, and the check is repeated before the server-side pairing
        // record is consumed.
        guard.store().ensure_unpinned(server, &cli.server_name)?;

        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create the pairing runtime")?;
        let fingerprint = runtime.block_on(async {
            let transport =
                quickfs_transport_quic::PairingClient::connect(server, &cli.server_name, timeout)
                    .await
                    .context("failed to open the temporary pairing connection")?;
            let fingerprint = quickfs_client_core::verify_pairing(&transport, pairing_id, &code)
                .await
                .context("server pairing verification failed")?;
            transport.close();
            Ok::<_, anyhow::Error>(fingerprint)
        })?;
        guard.pin(server, &cli.server_name, fingerprint)?;
        println!("Paired with {} ({server})", cli.server_name);
        println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
        Ok(())
    }

    fn run_forget(cli: &Cli, server: SocketAddr) -> Result<()> {
        let store = TrustStore::open(&cli.state_dir)?;
        store.lock()?.forget(server, &cli.server_name)?;
        println!(
            "Removed pinned identity for {} ({server}).",
            cli.server_name
        );
        println!("A new connection will require pairing or another managed trust method.");
        Ok(())
    }

    fn run_trust(cli: &Cli, command: &TrustCommand) -> Result<()> {
        match command {
            TrustCommand::List => {
                let pins = TrustStore::open(&cli.state_dir)?.list()?;
                if pins.is_empty() {
                    println!("No pinned server identities.");
                    return Ok(());
                }
                for pin in pins {
                    println!(
                        "{} ({})  SHA-256 {}",
                        pin.server_name,
                        pin.address,
                        hex::encode(pin.certificate_sha256)
                    );
                }
                Ok(())
            }
            TrustCommand::Import {
                server,
                sha256,
                certificate,
            } => {
                let fingerprint = match (sha256, certificate) {
                    (Some(fingerprint), None) => parse_sha256_fingerprint(fingerprint)?,
                    (None, Some(path)) => {
                        let certificates = load_certificates(path).with_context(|| {
                            format!("failed to load certificate '{}'", path.display())
                        })?;
                        certificate_sha256(
                            certificates
                                .first()
                                .ok_or_else(|| anyhow::anyhow!("certificate bundle is empty"))?,
                        )
                    }
                    _ => bail!("supply exactly one of --sha256 or --certificate"),
                };
                let store = TrustStore::open(&cli.state_dir)?;
                let guard = store.lock()?;
                guard.store().ensure_unpinned(*server, &cli.server_name)?;
                guard.pin(*server, &cli.server_name, fingerprint)?;
                println!("Imported managed pin for {} ({server}).", cli.server_name);
                println!("Pinned certificate SHA-256: {}", hex::encode(fingerprint));
                Ok(())
            }
        }
    }

    fn run_unmount(mountpoint: &str) -> Result<()> {
        let mountpoint = MountPoint::parse(mountpoint)?;
        if signal_unmount(&mountpoint.identifier())? {
            println!("Asked the client holding {mountpoint} to unmount.");
            Ok(())
        } else {
            bail!(
                "no quicKFS mount is active on {mountpoint} in this session. \
                 A mount started by another user or session can only be stopped \
                 from that session"
            )
        }
    }

    fn run_mount(cli: &Cli) -> Result<()> {
        let Command::Mount {
            server,
            mountpoint,
            username,
            cache_dir,
            cache_max_bytes,
            cache_block_kib,
            read_ahead_max_bytes,
            transport_timeout_ms,
            callback_timeout_ms,
            reconnect_attempts,
            reconnect_initial_backoff_ms,
            reconnect_max_backoff_ms,
            volume_name,
        } = &cli.command
        else {
            bail!("internal error: mount dispatched for another command");
        };

        if *transport_timeout_ms == 0 || *callback_timeout_ms == 0 {
            bail!("transport and callback timeouts must be greater than zero");
        }
        if *cache_max_bytes == 0
            || *cache_block_kib == 0
            || *reconnect_attempts == 0
            || reconnect_initial_backoff_ms > reconnect_max_backoff_ms
        {
            bail!(
                "cache sizes and reconnect attempts must be positive, and reconnect backoff must not decrease"
            );
        }
        let mountpoint = MountPoint::parse(mountpoint)?;
        let username = username
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--username or QUICKFS_USERNAME is required"))?;
        let trust = server_trust(cli, *server)?;
        let transport_timeout = Duration::from_millis(*transport_timeout_ms);
        let operation_deadline = Duration::from_millis(*callback_timeout_ms);

        // Claim the mountpoint before doing any network work, so a second
        // `quickfs mount Q:` fails immediately instead of after a password
        // prompt and a full connection.
        let unmount_signal = UnmountSignal::create(&mountpoint.identifier()).map_err(|_| {
            anyhow::anyhow!("{mountpoint} already has a quicKFS mount in this session")
        })?;

        // The WinFsp DLL is delay loaded, so it has to be brought in explicitly
        // before the first call into it.
        winfsp_wrs::init().map_err(|error| anyhow::anyhow!("failed to load WinFsp: {error}"))?;

        let runtime = Arc::new(
            Builder::new_multi_thread()
                .enable_all()
                .thread_name("quickfs-remote")
                .build()
                .context("failed to create the shared Tokio runtime")?,
        );

        // Authenticate the server *before* asking for a password, exactly as
        // the macOS client does, so a credential is never offered to a server
        // whose identity has not been established.
        let identity_check = runtime
            .block_on(trust.connect(*server, &cli.server_name, transport_timeout))
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
            *server,
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
                        attempts: *reconnect_attempts,
                        initial_backoff: Duration::from_millis(*reconnect_initial_backoff_ms),
                        maximum_backoff: Duration::from_millis(*reconnect_max_backoff_ms),
                    },
                    operation_deadline,
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
        let cache_root = cache_dir.clone().unwrap_or_else(|| cli.state_dir.clone());
        let cache = Arc::new(
            NonBlockingPersistentCache::open(&cache_root, cache_namespace, *cache_max_bytes)
                .with_context(|| {
                    format!(
                        "failed to open persistent cache at '{}'",
                        cache_root.display()
                    )
                })?,
        );
        let cache_block_size = cache_block_kib
            .checked_mul(1024)
            .context("cache block size is too large")?
            .min(capabilities.max_read_size)
            .min(MAX_CLIENT_READ_SIZE);
        let filesystem = CachedFilesystem::new(
            resilient,
            cache,
            CachePolicy {
                block_size: cache_block_size,
                read_ahead_max_bytes: *read_ahead_max_bytes,
            },
        )
        .context("failed to configure the filesystem cache")?;
        let remote: Arc<dyn RemoteFilesystem> = Arc::new(filesystem);
        let adapter = quickfs_filesystem_winfsp::Adapter::with_runtime(
            remote,
            operation_deadline.saturating_add(DEADLINE_ERROR_DELIVERY_GRACE),
            runtime,
        );

        let owner_sid = quickfs_windows_support::current_user_sid()
            .context("failed to read the mounting account's identity")?;
        let config = MountConfig {
            mountpoint: mountpoint.clone(),
            volume_name: volume_name.clone(),
            filesystem_name: "quickfs".into(),
        };
        let access = if capabilities.writable {
            "read/write"
        } else {
            "read-only"
        };
        let mounted = mount(adapter, capabilities, &config, owner_sid)
            .with_context(|| format!("failed to mount on {mountpoint}"))?;
        println!(
            "Mounted {access} {} ({server}) on {mountpoint} with reconnect and offline read \
             caching. Press Control+C, or run `quickfs unmount {mountpoint}`, to unmount.",
            cli.server_name
        );

        wait_for_shutdown(unmount_signal)?;
        println!("Unmounting {mountpoint}...");
        mounted.stop();
        Ok(())
    }

    /// Block until the user asks the mount to stop, by either route.
    ///
    /// Both routes have to be watched at once: Control+C is what an interactive
    /// user reaches for, and the named event is the only way another window can
    /// reach a mount that lives inside this process.
    fn wait_for_shutdown(signal: UnmountSignal) -> Result<()> {
        let (sender, receiver) = mpsc::channel::<()>();
        let interrupt = sender.clone();
        ctrlc_handler(move || {
            let _ = interrupt.send(());
        })?;
        std::thread::spawn(move || {
            if signal.wait().is_ok() {
                let _ = sender.send(());
            }
        });
        // A send error means every sender was dropped, which cannot happen
        // while the mount is running; treat it as a stop request either way
        // rather than spinning.
        let _ = receiver.recv();
        Ok(())
    }

    /// Install a Control+C handler.
    ///
    /// `ctrl_c` is delivered on a runtime thread, so the handler only signals a
    /// channel; the unmount itself happens on the main thread, where the
    /// `Mount` lives.
    fn ctrlc_handler(handler: impl Fn() + Send + 'static) -> Result<()> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to create the signal runtime")?;
        std::thread::spawn(move || {
            runtime.block_on(async {
                if tokio::signal::ctrl_c().await.is_ok() {
                    handler();
                }
            });
        });
        Ok(())
    }

    fn server_trust(cli: &Cli, server: SocketAddr) -> Result<ServerTrust> {
        if cli.trust_system_roots {
            return Ok(ServerTrust::system_roots());
        }
        if let Some(path) = &cli.ca_cert {
            let authorities = load_certificates(path).with_context(|| {
                format!("failed to load enterprise CA bundle '{}'", path.display())
            })?;
            return Ok(ServerTrust::enterprise_ca(authorities));
        }
        let fingerprint = load_trusted_server_pin(&cli.state_dir, server, &cli.server_name)
            .context(
                "no exact pin is configured; run `quickfs pair` or `quickfs trust import`, \
                 or use --ca-cert/--trust-system-roots for centrally managed PKI",
            )?;
        Ok(ServerTrust::pinned(fingerprint))
    }

    /// WinFsp stores the volume label in a fixed 32-unit buffer, so a longer
    /// name is refused here rather than silently truncated into something the
    /// user did not choose.
    fn parse_volume_name(value: &str) -> std::result::Result<String, String> {
        let units = value.encode_utf16().count();
        if value.is_empty() || units > 32 || value.contains('\0') {
            Err("volume name must contain 1 to 32 characters and no NUL".into())
        } else {
            Ok(value.to_owned())
        }
    }

    #[cfg(test)]
    #[allow(clippy::unwrap_used)]
    mod tests {
        use super::*;
        use clap::CommandFactory;

        #[test]
        fn the_command_line_matches_the_documented_shape() {
            Cli::command().debug_assert();
        }

        #[test]
        fn mounting_takes_a_server_and_a_mountpoint_positionally() {
            let cli = Cli::try_parse_from([
                "quickfs",
                "mount",
                "192.0.2.10:4433",
                "Q:",
                "--username",
                "alice",
            ])
            .unwrap();
            let Command::Mount {
                server,
                mountpoint,
                username,
                ..
            } = &cli.command
            else {
                panic!("expected a mount command");
            };
            assert_eq!(server.to_string(), "192.0.2.10:4433");
            assert_eq!(mountpoint, "Q:");
            assert_eq!(username.as_deref(), Some("alice"));
        }

        #[test]
        fn unmounting_takes_only_a_mountpoint() {
            let cli = Cli::try_parse_from(["quickfs", "unmount", "Q:"]).unwrap();
            let Command::Unmount { mountpoint } = &cli.command else {
                panic!("expected an unmount command");
            };
            assert_eq!(mountpoint, "Q:");
        }

        #[test]
        fn the_enterprise_trust_switches_are_mutually_exclusive() {
            assert!(
                Cli::try_parse_from([
                    "quickfs",
                    "--trust-system-roots",
                    "--ca-cert",
                    "root.pem",
                    "mount",
                    "192.0.2.10:4433",
                    "Q:",
                ])
                .is_err()
            );
        }

        #[test]
        fn importing_a_pin_requires_exactly_one_source() {
            assert!(
                Cli::try_parse_from(["quickfs", "trust", "import", "192.0.2.10:4433"]).is_err()
            );
            assert!(
                Cli::try_parse_from([
                    "quickfs",
                    "trust",
                    "import",
                    "192.0.2.10:4433",
                    "--sha256",
                    &"07".repeat(32),
                    "--certificate",
                    "server.pem",
                ])
                .is_err()
            );
            assert!(
                Cli::try_parse_from([
                    "quickfs",
                    "trust",
                    "import",
                    "192.0.2.10:4433",
                    "--sha256",
                    &"07".repeat(32),
                ])
                .is_ok()
            );
        }

        #[test]
        fn a_volume_label_longer_than_windows_stores_is_refused() {
            assert!(parse_volume_name("quicKFS").is_ok());
            assert!(parse_volume_name(&"a".repeat(32)).is_ok());
            assert!(parse_volume_name(&"a".repeat(33)).is_err());
            assert!(parse_volume_name("").is_err());
            // Non-BMP characters cost two units each.
            assert!(parse_volume_name(&"\u{1F600}".repeat(17)).is_err());
        }

        #[test]
        fn trust_commands_refuse_the_flags_that_do_not_apply_to_them() {
            let cli = Cli::try_parse_from([
                "quickfs",
                "--trust-system-roots",
                "forget",
                "192.0.2.10:4433",
            ])
            .unwrap();
            assert!(reject_enterprise_flags(&cli).is_err());
        }
    }
}
