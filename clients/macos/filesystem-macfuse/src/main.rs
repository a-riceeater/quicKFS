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
    use clap::Parser;
    use quickfs_client_core::{
        NetworkFilesystem, RemoteFilesystem, ServerTrust, load_trusted_server_pin,
    };
    use quickfs_filesystem_macfuse::{Adapter, MountConfig, mount};
    use quickfs_transport_quic::load_certificates;
    use std::{
        net::SocketAddr,
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::runtime::Builder;
    use zeroize::Zeroizing;

    #[derive(Parser)]
    #[command(
        name = "quickfs-mount",
        about = "Mount an authenticated quicKFS export as a read-only macFUSE volume"
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
        #[arg(long, default_value_t = 30_000)]
        timeout_ms: u64,
        #[arg(long, default_value_t = 30_000)]
        callback_timeout_ms: u64,
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
    }

    pub fn run() -> Result<()> {
        let cli = Cli::parse();
        quickfs_macos_support::require_macfuse()?;
        if cli.timeout_ms == 0 || cli.callback_timeout_ms == 0 {
            bail!("transport and callback timeouts must be greater than zero");
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
        identity_check.close();

        let password = Zeroizing::new(rpassword::prompt_password("Password: ")?);
        // Reconnect after the potentially long prompt and reapply exactly the
        // same trust policy before transmitting the credential.
        let transport = runtime
            .block_on(trust.connect(cli.server, &cli.server_name, transport_timeout))
            .with_context(|| {
                format!(
                    "failed to re-authenticate server via {}; password was not sent",
                    trust.description()
                )
            })?;
        let filesystem = runtime
            .block_on(NetworkFilesystem::authenticate(
                transport,
                username,
                password.to_string(),
            ))
            .context("username/password authentication failed")?;
        let remote: Arc<dyn RemoteFilesystem> = Arc::new(filesystem);
        let adapter = Adapter::with_runtime(
            remote,
            Duration::from_millis(cli.callback_timeout_ms),
            runtime,
        );
        let mount_config = MountConfig {
            volume_name: cli.volume_name,
            filesystem_name: "quickfs".into(),
        };

        println!(
            "Mounting read-only {} ({}) at {}. Unmount with `diskutil unmount '{}'`.",
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
