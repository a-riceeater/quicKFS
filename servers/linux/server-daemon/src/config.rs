// SPDX-License-Identifier: Apache-2.0
//! TOML configuration for the Linux server daemon.
//!
//! One resolution step produces every effective value so a Clap default can
//! never silently overwrite a configured one. Raw command-line fields are
//! `Option`, Clap fills them from `--flag` or the documented `QUICKFS_*`
//! environment variable, and [`resolve`] applies the precedence:
//!
//! ```text
//! explicit CLI option > environment variable > TOML value > built-in default
//! ```
//!
//! The file model uses `deny_unknown_fields` throughout, so a misspelled
//! security-relevant key (`allow_writess`, `[limit]`) is an error rather than a
//! silently ignored setting that leaves the built-in default in place.

use anyhow::{Context, Result, bail};
use quickfs_auth::{StatePaths, validate_server_state};
use quickfs_common::{DEFAULT_MAX_READ_SIZE, DEFAULT_MAX_WRITE_SIZE};
use serde::Deserialize;
use std::{
    fmt::Write as _,
    fs::File,
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tokio::sync::Semaphore;

/// System configuration path used when neither `--config` nor `QUICKFS_CONFIG`
/// selects another file.
pub const DEFAULT_CONFIG_PATH: &str = "/etc/quickfs/quickfs.toml";

/// Largest configuration file the daemon will read. A configuration file is a
/// small hand-maintained document; anything larger is a mistake or an attempt
/// to make the parser allocate.
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub const DEFAULT_BIND: &str = "0.0.0.0:4433";
pub const DEFAULT_STATE_DIR: &str = ".quickfs";
pub const DEFAULT_ALLOW_WRITES: bool = false;
pub const DEFAULT_MAX_OPEN_HANDLES: usize = 1024;
pub const DEFAULT_MAX_KNOWN_NODES_PER_CONNECTION: usize = 131_072;
pub const DEFAULT_MAX_TOTAL_KNOWN_NODES: usize = 524_288;
pub const DEFAULT_MAX_DIRECTORY_ENTRY_TASKS: usize = 64;
pub const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 128;
pub const DEFAULT_MAX_CONCURRENT_REQUESTS_PER_CONNECTION: usize = 64;
pub const DEFAULT_MAX_IN_FLIGHT_READ_BYTES: usize = 128 * 1024 * 1024;
pub const DEFAULT_MAX_IN_FLIGHT_WRITE_BYTES: usize = 64 * 1024 * 1024;
pub const DEFAULT_MAX_IN_FLIGHT_WRITE_BYTES_PER_CONNECTION: usize = 16 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_CONNECTIONS: usize = 256;
pub const DEFAULT_MAX_CONCURRENT_AUTH: usize = 4;
pub const DEFAULT_AUTH_ATTEMPTS_PER_MINUTE: usize = 30;

/// Placeholder emitted by `config print-effective` in place of a secret.
///
/// No configuration field carries a secret today: passwords, pairing codes and
/// private keys deliberately live in protected server state rather than in a
/// file an administrator edits and pastes into a bug report. Any future
/// secret-bearing field must be printed through this constant so
/// `print-effective` output stays safe to share.
const REDACTED: &str = "<redacted>";

// ---------------------------------------------------------------------------
// File model
// ---------------------------------------------------------------------------

/// The parsed contents of a `quickfs.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub limits: LimitsSection,
    #[serde(default)]
    pub logging: LoggingSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    pub bind: Option<String>,
    pub export_root: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub allow_writes: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsSection {
    pub max_read_size: Option<u64>,
    pub max_write_size: Option<u64>,
    pub max_open_handles: Option<usize>,
    pub max_known_nodes_per_connection: Option<usize>,
    pub max_total_known_nodes: Option<usize>,
    pub max_directory_entry_tasks: Option<usize>,
    pub request_timeout_ms: Option<u64>,
    pub max_concurrent_requests: Option<usize>,
    pub max_concurrent_requests_per_connection: Option<usize>,
    pub max_in_flight_read_bytes: Option<usize>,
    pub max_in_flight_write_bytes: Option<usize>,
    pub max_in_flight_write_bytes_per_connection: Option<usize>,
    pub max_concurrent_connections: Option<usize>,
    pub max_concurrent_auth: Option<usize>,
    pub auth_attempts_per_minute: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    /// `tracing-subscriber` filter directive, for example `info` or
    /// `quickfs_server_daemon=debug`. An explicit `RUST_LOG` overrides it.
    pub filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Read and parse a configuration file, rejecting unsafe file shapes first.
///
/// The file is rejected when it is a symbolic link, is not a regular file, or
/// is writable by group or other users: anything able to rewrite the daemon's
/// configuration can point `export_root` at `/` or move `state_dir` onto a
/// directory it controls, so this is a privilege boundary rather than a style
/// preference. The path is stat'ed, opened, and then re-stat'ed through the
/// open descriptor with the device/inode compared, which closes the ordinary
/// check/use race between the two steps.
pub fn load(path: &Path) -> Result<FileConfig> {
    let text = read_config_file(path)
        .with_context(|| format!("failed to read configuration '{}'", path.display()))?;
    let file: FileConfig = toml::from_str(&text)
        .with_context(|| format!("failed to parse configuration '{}'", path.display()))?;
    validate_file_paths(&file, path)?;
    Ok(file)
}

fn read_config_file(path: &Path) -> Result<String> {
    let link = std::fs::symlink_metadata(path)?;
    if link.file_type().is_symlink() {
        bail!(
            "configuration file '{}' is a symbolic link; the daemon reads its configuration only through a real file",
            path.display()
        );
    }
    if !link.file_type().is_file() {
        bail!(
            "configuration file '{}' is not a regular file",
            path.display()
        );
    }
    let mut file = File::open(path)?;
    let opened = file.metadata()?;
    if !opened.file_type().is_file() {
        bail!(
            "configuration file '{}' is not a regular file",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if opened.dev() != link.dev() || opened.ino() != link.ino() {
            bail!(
                "configuration file '{}' was replaced while it was being opened",
                path.display()
            );
        }
        if opened.permissions().mode() & 0o022 != 0 {
            bail!(
                "configuration file '{}' is writable by group or other users; use mode 0640 (root:quickfs) for a system installation",
                path.display()
            );
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let directory = std::fs::metadata(parent)?;
            let mode = directory.permissions().mode();
            // A world-writable parent lets any local account swap the file
            // between validation and use. A sticky world-writable directory
            // (`/tmp`) still protects other accounts' entries, so it is allowed.
            if mode & 0o002 != 0 && mode & 0o1000 == 0 {
                bail!(
                    "configuration directory '{}' is writable by other users",
                    parent.display()
                );
            }
        }
    }
    if opened.len() > MAX_CONFIG_BYTES {
        bail!(
            "configuration file '{}' is larger than {MAX_CONFIG_BYTES} bytes",
            path.display()
        );
    }
    let mut text = String::new();
    file.by_ref()
        .take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        bail!(
            "configuration file '{}' is larger than {MAX_CONFIG_BYTES} bytes",
            path.display()
        );
    }
    Ok(text)
}

/// A configuration file describes a deployment, so its paths must be absolute.
///
/// A relative path in a system service resolves against whatever working
/// directory the supervisor happened to use. Development invocations that pass
/// `--export-root ./shared` on the command line are unaffected.
fn validate_file_paths(file: &FileConfig, path: &Path) -> Result<()> {
    for (name, value) in [
        ("server.export_root", file.server.export_root.as_ref()),
        ("server.state_dir", file.server.state_dir.as_ref()),
    ] {
        if let Some(value) = value
            && !value.is_absolute()
        {
            bail!(
                "{name} in '{}' must be an absolute path, found '{}'",
                path.display(),
                value.display()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The command-line half of `serve`, before any default is applied.
///
/// Every field is optional and carries no Clap default, so `None` means "the
/// administrator said nothing here" and the TOML value (then the built-in
/// default) is free to supply it. Clap resolves `--flag` above the documented
/// environment variable for us, which is the first two levels of the
/// precedence chain.
#[derive(clap::Args, Clone, Debug, Default)]
#[command(
    about = "Serve the configured export over QUIC",
    long_about = "Serve the configured export over QUIC.\n\n\
Every option below falls back to the configuration file and then to the \
built-in default, so leaving one out does not pin it to a default value. Run \
`quickfs-server-daemon config print-effective` to see what an invocation \
actually resolves to."
)]
pub struct ServeArgs {
    /// Configuration file. Defaults to `/etc/quickfs/quickfs.toml` when it
    /// exists; an explicitly named file must exist.
    #[arg(long, env = "QUICKFS_CONFIG", value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, env = "QUICKFS_BIND")]
    pub bind: Option<SocketAddr>,
    #[arg(long, env = "QUICKFS_EXPORT_ROOT")]
    pub export_root: Option<PathBuf>,
    #[arg(long, env = "QUICKFS_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    /// Permit mutating filesystem requests. Exports are read-only by default.
    ///
    /// `--allow-writes` alone means true and `--allow-writes=false` turns it
    /// back off, while omitting it entirely leaves the decision to the
    /// configuration file. It deliberately has no Clap default: a default here
    /// would outrank `allow_writes` in the TOML file and decide the export-wide
    /// write gate on the administrator's behalf.
    #[arg(
        long,
        env = "QUICKFS_ALLOW_WRITES",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true"
    )]
    pub allow_writes: Option<bool>,
    #[arg(long)]
    pub max_read_size: Option<u64>,
    #[arg(long)]
    pub max_write_size: Option<u64>,
    #[arg(long)]
    pub max_open_handles: Option<usize>,
    // The per-connection ceiling on distinct inodes one client may reference at
    // once (its live working set), not a limit on files in the export. macOS
    // keeps a large kernel vnode cache, so a Finder/Spotlight/`find` crawl over
    // a big media library legitimately holds tens of thousands of live inodes on
    // a single mount, and `auto_xattr` roughly doubles that by tracking `._`
    // AppleDouble sidecars. 8192 (the historical default) was sized for small
    // exports and starved such workloads with "too many known nodes". The
    // semaphore backing this is counter-based, so a higher ceiling costs nothing
    // until nodes are actually tracked (~400-500 bytes each). See docs/protocol.md.
    #[arg(long)]
    pub max_known_nodes_per_connection: Option<usize>,
    #[arg(long)]
    pub max_total_known_nodes: Option<usize>,
    /// Aggregate metadata/xattr workers used across directory requests.
    #[arg(long)]
    pub max_directory_entry_tasks: Option<usize>,
    /// End-to-end request deadline, including cold backing-store work and raw
    /// payload transfer. This must comfortably exceed the mount's transport
    /// phase deadline so a slow read is not canceled while still progressing.
    #[arg(long)]
    pub request_timeout_ms: Option<u64>,
    #[arg(long)]
    pub max_concurrent_requests: Option<usize>,
    /// Concurrent request tasks one connection may occupy. Keeps a single
    /// slow or hostile client from holding every global request slot.
    #[arg(long)]
    pub max_concurrent_requests_per_connection: Option<usize>,
    #[arg(long)]
    pub max_in_flight_read_bytes: Option<usize>,
    /// Aggregate memory reserved for write request bodies across all clients.
    #[arg(long)]
    pub max_in_flight_write_bytes: Option<usize>,
    /// Memory reserved for write bodies from any one client connection.
    #[arg(long)]
    pub max_in_flight_write_bytes_per_connection: Option<usize>,
    #[arg(long)]
    pub max_concurrent_connections: Option<usize>,
    #[arg(long)]
    pub max_concurrent_auth: Option<usize>,
    #[arg(long)]
    pub auth_attempts_per_minute: Option<usize>,
}

/// Every effective `serve` value, after precedence has been applied.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub export_root: PathBuf,
    pub state_dir: PathBuf,
    pub allow_writes: bool,
    pub max_read_size: u64,
    pub max_write_size: u64,
    pub max_open_handles: usize,
    pub max_known_nodes_per_connection: usize,
    pub max_total_known_nodes: usize,
    pub max_directory_entry_tasks: usize,
    pub request_timeout_ms: u64,
    pub max_concurrent_requests: usize,
    pub max_concurrent_requests_per_connection: usize,
    pub max_in_flight_read_bytes: usize,
    pub max_in_flight_write_bytes: usize,
    pub max_in_flight_write_bytes_per_connection: usize,
    pub max_concurrent_connections: usize,
    pub max_concurrent_auth: usize,
    pub auth_attempts_per_minute: usize,
    /// `logging.filter` from the file, if any. An explicit `RUST_LOG` wins.
    pub log_filter: Option<String>,
    /// The configuration file these values came from, when one was read.
    pub source: Option<PathBuf>,
}

/// Decide which configuration file to read, if any.
///
/// An explicitly named file (`--config`, `QUICKFS_CONFIG`) must exist. The
/// system default is used when it is present and ignored when it is not, so a
/// development `serve --export-root ./shared` works on a machine that also has
/// a system installation absent.
pub fn select_file(explicit: Option<&Path>) -> Result<Option<PathBuf>> {
    match explicit {
        Some(path) => {
            if !path.exists() {
                bail!("configuration file '{}' does not exist", path.display());
            }
            Ok(Some(path.to_path_buf()))
        }
        None => {
            let default = Path::new(DEFAULT_CONFIG_PATH);
            Ok(default.exists().then(|| default.to_path_buf()))
        }
    }
}

/// Apply `CLI > environment > TOML > built-in default` to produce the effective
/// configuration.
pub fn resolve(args: &ServeArgs, file: Option<(PathBuf, FileConfig)>) -> Result<ServerConfig> {
    let (source, file) = match file {
        Some((path, file)) => (Some(path), file),
        None => (None, FileConfig::default()),
    };
    let bind = match args.bind {
        Some(bind) => bind,
        None => match file.server.bind.as_deref() {
            Some(text) => text
                .parse::<SocketAddr>()
                .with_context(|| format!("server.bind '{text}' is not an IP address and port"))?,
            None => DEFAULT_BIND
                .parse::<SocketAddr>()
                .context("the built-in bind address is invalid")?,
        },
    };
    let export_root = args
        .export_root
        .clone()
        .or(file.server.export_root)
        .context(
            "no export root configured; set server.export_root in the configuration file or pass --export-root",
        )?;
    let limits = file.limits;
    Ok(ServerConfig {
        bind,
        export_root,
        state_dir: args
            .state_dir
            .clone()
            .or(file.server.state_dir)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR)),
        allow_writes: args
            .allow_writes
            .or(file.server.allow_writes)
            .unwrap_or(DEFAULT_ALLOW_WRITES),
        max_read_size: args
            .max_read_size
            .or(limits.max_read_size)
            .unwrap_or(DEFAULT_MAX_READ_SIZE),
        max_write_size: args
            .max_write_size
            .or(limits.max_write_size)
            .unwrap_or(DEFAULT_MAX_WRITE_SIZE),
        max_open_handles: args
            .max_open_handles
            .or(limits.max_open_handles)
            .unwrap_or(DEFAULT_MAX_OPEN_HANDLES),
        max_known_nodes_per_connection: args
            .max_known_nodes_per_connection
            .or(limits.max_known_nodes_per_connection)
            .unwrap_or(DEFAULT_MAX_KNOWN_NODES_PER_CONNECTION),
        max_total_known_nodes: args
            .max_total_known_nodes
            .or(limits.max_total_known_nodes)
            .unwrap_or(DEFAULT_MAX_TOTAL_KNOWN_NODES),
        max_directory_entry_tasks: args
            .max_directory_entry_tasks
            .or(limits.max_directory_entry_tasks)
            .unwrap_or(DEFAULT_MAX_DIRECTORY_ENTRY_TASKS),
        request_timeout_ms: args
            .request_timeout_ms
            .or(limits.request_timeout_ms)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_MS),
        max_concurrent_requests: args
            .max_concurrent_requests
            .or(limits.max_concurrent_requests)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS),
        max_concurrent_requests_per_connection: args
            .max_concurrent_requests_per_connection
            .or(limits.max_concurrent_requests_per_connection)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_REQUESTS_PER_CONNECTION),
        max_in_flight_read_bytes: args
            .max_in_flight_read_bytes
            .or(limits.max_in_flight_read_bytes)
            .unwrap_or(DEFAULT_MAX_IN_FLIGHT_READ_BYTES),
        max_in_flight_write_bytes: args
            .max_in_flight_write_bytes
            .or(limits.max_in_flight_write_bytes)
            .unwrap_or(DEFAULT_MAX_IN_FLIGHT_WRITE_BYTES),
        max_in_flight_write_bytes_per_connection: args
            .max_in_flight_write_bytes_per_connection
            .or(limits.max_in_flight_write_bytes_per_connection)
            .unwrap_or(DEFAULT_MAX_IN_FLIGHT_WRITE_BYTES_PER_CONNECTION),
        max_concurrent_connections: args
            .max_concurrent_connections
            .or(limits.max_concurrent_connections)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_CONNECTIONS),
        max_concurrent_auth: args
            .max_concurrent_auth
            .or(limits.max_concurrent_auth)
            .unwrap_or(DEFAULT_MAX_CONCURRENT_AUTH),
        auth_attempts_per_minute: args
            .auth_attempts_per_minute
            .or(limits.auth_attempts_per_minute)
            .unwrap_or(DEFAULT_AUTH_ATTEMPTS_PER_MINUTE),
        log_filter: file.logging.filter,
        source,
    })
}

/// Load the file selected by `args` and resolve the effective configuration.
pub fn load_and_resolve(args: &ServeArgs) -> Result<ServerConfig> {
    let selected = select_file(args.config.as_deref())?;
    let file = match selected {
        Some(path) => {
            let parsed = load(&path)?;
            Some((path, parsed))
        }
        None => None,
    };
    resolve(args, file)
}

/// Resolve the state directory for an administrative command.
///
/// `explicit` is the command's own `--state-dir`/`QUICKFS_STATE_DIR` value and
/// always wins, so `user add --config … --state-dir /tmp/staging` still targets
/// the directory the administrator named.
pub fn resolve_state_dir(explicit: Option<&Path>, config: Option<&Path>) -> Result<PathBuf> {
    if let Some(explicit) = explicit {
        return Ok(explicit.to_path_buf());
    }
    if let Some(path) = select_file(config)? {
        let file = load(&path)?;
        if let Some(state_dir) = file.server.state_dir {
            return Ok(state_dir);
        }
    }
    Ok(PathBuf::from(DEFAULT_STATE_DIR))
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

impl ServerConfig {
    /// Reject zero, contradictory, or unrepresentable limits.
    ///
    /// This runs before anything is opened or bound, so an invalid unit file or
    /// a mistyped limit fails the service start rather than producing a daemon
    /// with a nonsensical budget.
    pub fn validate_limits(&self) -> Result<()> {
        let c = self;
        if c.max_read_size == 0 {
            bail!("maximum read size must be greater than zero");
        }
        if c.max_write_size == 0 {
            bail!("maximum write size must be greater than zero");
        }
        if c.max_open_handles == 0 {
            bail!("maximum open handles must be greater than zero");
        }
        if c.max_known_nodes_per_connection == 0 {
            bail!("maximum known nodes per connection must be greater than zero");
        }
        if c.max_total_known_nodes == 0
            || c.max_known_nodes_per_connection.saturating_sub(1) > c.max_total_known_nodes
        {
            bail!("total known-node capacity must cover at least one connection");
        }
        if c.request_timeout_ms == 0 {
            bail!("request timeout must be greater than zero milliseconds");
        }
        if c.max_directory_entry_tasks == 0 {
            bail!("maximum directory entry tasks must be greater than zero");
        }
        if c.max_concurrent_requests == 0 {
            bail!("maximum concurrent requests must be greater than zero");
        }
        if c.max_concurrent_requests_per_connection == 0 {
            bail!("maximum concurrent requests per connection must be greater than zero");
        }
        if c.max_in_flight_read_bytes == 0 {
            bail!("maximum in-flight read bytes must be greater than zero");
        }
        if c.max_in_flight_write_bytes == 0 || c.max_in_flight_write_bytes_per_connection == 0 {
            bail!(
                "global and per-connection in-flight write-byte budgets must be greater than zero"
            );
        }
        if c.max_read_size > u32::MAX.into()
            || usize::try_from(c.max_read_size).is_err()
            || c.max_read_size as usize > c.max_in_flight_read_bytes
        {
            bail!(
                "maximum read size must fit within the configured in-flight read-byte budget and u32"
            );
        }
        if c.max_write_size > u32::MAX.into()
            || usize::try_from(c.max_write_size).is_err()
            || c.max_write_size as usize > c.max_in_flight_write_bytes
            || c.max_write_size as usize > c.max_in_flight_write_bytes_per_connection
        {
            bail!("maximum write size must fit within both in-flight write-byte budgets and u32");
        }
        if c.max_in_flight_write_bytes_per_connection > c.max_in_flight_write_bytes {
            bail!("per-connection write-byte budget must not exceed the global write-byte budget");
        }
        if c.max_open_handles > Semaphore::MAX_PERMITS
            || c.max_known_nodes_per_connection > Semaphore::MAX_PERMITS
            || c.max_total_known_nodes > Semaphore::MAX_PERMITS
            || c.max_directory_entry_tasks > Semaphore::MAX_PERMITS
            || c.max_concurrent_requests > Semaphore::MAX_PERMITS
            || c.max_concurrent_requests_per_connection > Semaphore::MAX_PERMITS
            || c.max_concurrent_connections > Semaphore::MAX_PERMITS
            || c.max_concurrent_auth > Semaphore::MAX_PERMITS
            || c.max_in_flight_read_bytes > Semaphore::MAX_PERMITS
            || c.max_in_flight_write_bytes > Semaphore::MAX_PERMITS
            || c.max_in_flight_write_bytes_per_connection > Semaphore::MAX_PERMITS
        {
            bail!("configured concurrency exceeds the runtime semaphore limit");
        }
        if c.max_concurrent_connections == 0 {
            bail!("maximum concurrent connections must be greater than zero");
        }
        if c.max_concurrent_auth == 0 {
            bail!("maximum concurrent authentication tasks must be greater than zero");
        }
        if c.auth_attempts_per_minute == 0 {
            bail!("authentication attempts per minute must be greater than zero");
        }
        if c.auth_attempts_per_minute > 1_000 {
            bail!("authentication attempts per minute must not exceed 1000");
        }
        Ok(())
    }

    /// Resolve and cross-check the export root and state directory.
    ///
    /// Returns the canonical `(state_root, export_root)` pair. The two trees
    /// must be disjoint in both directions: a state directory inside the export
    /// would publish the private key, password database, and pairing records to
    /// every authenticated client, and an export inside the state directory
    /// would do the same through the other nesting.
    pub fn resolve_roots(&self) -> Result<(PathBuf, PathBuf)> {
        let export_metadata = std::fs::metadata(&self.export_root).with_context(|| {
            format!(
                "failed to open export root '{}'; ensure it exists and is readable",
                self.export_root.display()
            )
        })?;
        if !export_metadata.is_dir() {
            bail!(
                "export root '{}' is not a directory",
                self.export_root.display()
            );
        }
        let export_root = std::fs::canonicalize(&self.export_root).with_context(|| {
            format!(
                "failed to resolve export root '{}'",
                self.export_root.display()
            )
        })?;
        // The state directory legitimately does not exist yet before `init`, so
        // the overlap check resolves as much of it as exists rather than being
        // skipped. Comparing a canonical export root against a merely
        // absolutized state path would also miss an overlap hidden behind a
        // symlinked parent.
        let state_root = resolve_existing_prefix(&self.state_dir);
        if state_root.starts_with(&export_root) || export_root.starts_with(&state_root) {
            bail!(
                "server state directory '{}' overlaps export root '{}'; this could expose private authentication state",
                state_root.display(),
                export_root.display()
            );
        }
        Ok((state_root, export_root))
    }
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the rest.
///
/// `std::fs::canonicalize` fails outright on a path whose final components do
/// not exist yet, and its result is not directly comparable with a merely
/// absolutized path. Resolving the existing prefix gives one comparable form
/// for both roots and still follows a symlinked parent directory.
fn resolve_existing_prefix(path: &Path) -> PathBuf {
    let absolute = absolutize(path);
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut current = absolute.as_path();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(current) {
            let mut resolved = canonical;
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            return resolved;
        }
        let Some(parent) = current.parent() else {
            return absolute;
        };
        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        current = parent;
    }
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(current) => current.join(path),
        Err(_) => path.to_path_buf(),
    }
}

/// Everything `config validate` checks, with no mutation of any kind.
///
/// `require_identity` additionally demands initialized server state. The
/// systemd unit passes it from `ExecStartPre` so a service whose identity was
/// never created fails with that message instead of a TLS load error after the
/// unit has already been reported as starting.
pub fn validate(config: &ServerConfig, require_identity: bool) -> Result<()> {
    config.validate_limits()?;
    let (state_root, _export_root) = config.resolve_roots()?;
    if config.source.is_some() && !config.export_root.is_absolute() {
        bail!(
            "export root '{}' must be absolute in a configuration file",
            config.export_root.display()
        );
    }
    if let Some(filter) = &config.log_filter {
        quickfs_common::validate_log_filter(filter)
            .map_err(|error| anyhow::anyhow!("logging.filter '{filter}' is invalid: {error}"))?;
    }
    let initialized = StatePaths::new(&state_root).certificate.exists();
    if require_identity && !initialized {
        bail!(
            "server identity has not been initialized in '{}'; run `quickfs-server-daemon init --config <PATH> --server-name <NAME>` as the service user first",
            state_root.display()
        );
    }
    if initialized {
        validate_server_state(&state_root).context("server state permissions are unsafe")?;
    } else if state_root.exists() {
        validate_uninitialized_state_dir(&state_root)?;
    }
    Ok(())
}

/// A state directory that exists but holds no identity yet still has to be
/// private, because `init` will write a private key into it.
fn validate_uninitialized_state_dir(state_root: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(state_root)?;
    if !metadata.file_type().is_dir() {
        bail!(
            "server state directory '{}' is not a real directory",
            state_root.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "server state directory '{}' must not be accessible by group or other users (mode 0700)",
                state_root.display()
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Effective-configuration output
// ---------------------------------------------------------------------------

/// Render the resolved configuration as TOML that could be written back.
///
/// Secrets are never emitted: no field carries one today, and any that is added
/// must be printed as [`REDACTED`].
pub fn print_effective(config: &ServerConfig) -> String {
    let mut out = String::new();
    match &config.source {
        Some(path) => {
            let _ = writeln!(out, "# effective configuration from {}", path.display());
        }
        None => out.push_str("# effective configuration (no configuration file was read)\n"),
    }
    let _ = writeln!(
        out,
        "# values shown after: explicit CLI option > environment variable > TOML value > built-in default"
    );
    let _ = writeln!(out, "# secret-bearing values print as {REDACTED}");
    out.push_str("\n[server]\n");
    let _ = writeln!(out, "bind = {}", toml_string(&config.bind.to_string()));
    let _ = writeln!(
        out,
        "export_root = {}",
        toml_string(&config.export_root.display().to_string())
    );
    let _ = writeln!(
        out,
        "state_dir = {}",
        toml_string(&config.state_dir.display().to_string())
    );
    let _ = writeln!(out, "allow_writes = {}", config.allow_writes);
    out.push_str("\n[limits]\n");
    for (name, value) in [
        ("max_read_size", config.max_read_size as u128),
        ("max_write_size", config.max_write_size as u128),
        ("max_open_handles", config.max_open_handles as u128),
        (
            "max_known_nodes_per_connection",
            config.max_known_nodes_per_connection as u128,
        ),
        (
            "max_total_known_nodes",
            config.max_total_known_nodes as u128,
        ),
        (
            "max_directory_entry_tasks",
            config.max_directory_entry_tasks as u128,
        ),
        ("request_timeout_ms", config.request_timeout_ms as u128),
        (
            "max_concurrent_requests",
            config.max_concurrent_requests as u128,
        ),
        (
            "max_concurrent_requests_per_connection",
            config.max_concurrent_requests_per_connection as u128,
        ),
        (
            "max_in_flight_read_bytes",
            config.max_in_flight_read_bytes as u128,
        ),
        (
            "max_in_flight_write_bytes",
            config.max_in_flight_write_bytes as u128,
        ),
        (
            "max_in_flight_write_bytes_per_connection",
            config.max_in_flight_write_bytes_per_connection as u128,
        ),
        (
            "max_concurrent_connections",
            config.max_concurrent_connections as u128,
        ),
        ("max_concurrent_auth", config.max_concurrent_auth as u128),
        (
            "auth_attempts_per_minute",
            config.auth_attempts_per_minute as u128,
        ),
    ] {
        let _ = writeln!(out, "{name} = {value}");
    }
    out.push_str("\n[logging]\n");
    match &config.log_filter {
        Some(filter) => {
            let _ = writeln!(out, "filter = {}", toml_string(filter));
        }
        None => out.push_str("# filter is unset; RUST_LOG selects the level\n"),
    }
    out
}

/// Quote a value as a TOML basic string.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                let _ = write!(out, "\\u{:04X}", control as u32);
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn write(directory: &Path, text: &str) -> PathBuf {
        let path = directory.join("quickfs.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn args_with_config(path: &Path) -> ServeArgs {
        ServeArgs {
            config: Some(path.to_path_buf()),
            ..ServeArgs::default()
        }
    }

    #[test]
    fn a_complete_file_loads_and_resolves() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                r#"
[server]
bind = "127.0.0.1:14433"
export_root = {}
state_dir = {}
allow_writes = true

[limits]
max_read_size = 1048576
max_concurrent_auth = 2

[logging]
filter = "info"
"#,
                toml_string(&export.display().to_string()),
                toml_string(&directory.path().join("state").display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        assert_eq!(resolved.bind.port(), 14433);
        assert!(resolved.allow_writes);
        assert_eq!(resolved.max_read_size, 1_048_576);
        assert_eq!(resolved.max_concurrent_auth, 2);
        // Unset limits keep their built-in defaults rather than becoming zero.
        assert_eq!(resolved.max_write_size, DEFAULT_MAX_WRITE_SIZE);
        assert_eq!(resolved.log_filter.as_deref(), Some("info"));
        validate(&resolved, false).unwrap();
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            "[server]\nexport_root = \"/srv/export\"\nallow_writess = true\n",
        );
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("failed to parse"), "{error}");

        let path = write(directory.path(), "[limit]\nmax_read_size = 1\n");
        assert!(load(&path).is_err());
    }

    #[test]
    fn relative_paths_in_a_file_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(directory.path(), "[server]\nexport_root = \"export\"\n");
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("absolute"), "{error}");
    }

    #[test]
    fn invalid_limits_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\nstate_dir = {}\n[limits]\nmax_read_size = 0\n",
                toml_string(&export.display().to_string()),
                toml_string(&directory.path().join("state").display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        let error = validate(&resolved, false).unwrap_err().to_string();
        assert!(error.contains("maximum read size"), "{error}");
    }

    #[test]
    fn state_and_export_overlap_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\nstate_dir = {}\n",
                toml_string(&export.display().to_string()),
                toml_string(&export.join("state").display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        let error = validate(&resolved, false).unwrap_err().to_string();
        assert!(error.contains("overlaps export root"), "{error}");
    }

    #[test]
    fn a_missing_export_root_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\n",
                toml_string(&directory.path().join("absent").display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        assert!(validate(&resolved, false).is_err());
    }

    #[test]
    fn an_export_root_that_is_a_file_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        std::fs::write(&file, b"x").unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\n",
                toml_string(&file.display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        let error = validate(&resolved, false).unwrap_err().to_string();
        assert!(error.contains("not a directory"), "{error}");
    }

    #[test]
    fn requiring_identity_rejects_uninitialized_state() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\nstate_dir = {}\n",
                toml_string(&export.display().to_string()),
                toml_string(&directory.path().join("state").display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        validate(&resolved, false).unwrap();
        let error = validate(&resolved, true).unwrap_err().to_string();
        assert!(error.contains("has not been initialized"), "{error}");
    }

    #[test]
    fn precedence_runs_cli_over_environment_over_file_over_default() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nbind = \"127.0.0.1:15000\"\nexport_root = {}\n[limits]\nmax_open_handles = 77\n",
                toml_string(&export.display().to_string()),
            ),
        );
        let file = load(&path).unwrap();

        // The file supplies what the command line does not; unset values keep
        // the built-in default.
        let from_file = resolve(&args_with_config(&path), Some((path.clone(), file))).unwrap();
        assert_eq!(from_file.bind.port(), 15000);
        assert_eq!(from_file.max_open_handles, 77);
        assert_eq!(
            from_file.max_concurrent_auth, DEFAULT_MAX_CONCURRENT_AUTH,
            "an unset limit must not be overwritten by a Clap default"
        );

        // Clap resolves `--bind` above `QUICKFS_BIND` before we see the value,
        // so an explicit argument is simply `Some` and outranks the file.
        let file = load(&path).unwrap();
        let mut args = args_with_config(&path);
        args.bind = Some("127.0.0.1:16000".parse().unwrap());
        args.max_open_handles = Some(9);
        let from_cli = resolve(&args, Some((path.clone(), file))).unwrap();
        assert_eq!(from_cli.bind.port(), 16000);
        assert_eq!(from_cli.max_open_handles, 9);
    }

    #[test]
    fn a_missing_explicit_file_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("absent.toml");
        assert!(select_file(Some(&missing)).is_err());
    }

    #[test]
    fn print_effective_is_valid_toml_and_hides_no_secret() {
        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        std::fs::create_dir(&export).unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\n[logging]\nfilter = \"debug\"\n",
                toml_string(&export.display().to_string()),
            ),
        );
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        let rendered = print_effective(&resolved);
        let reparsed: FileConfig = toml::from_str(&rendered).unwrap();
        assert_eq!(reparsed.logging.filter.as_deref(), Some("debug"));
        assert_eq!(reparsed.limits.max_open_handles, Some(1024));
    }

    #[test]
    fn an_explicit_state_dir_outranks_the_configured_one() {
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            &format!(
                "[server]\nexport_root = {}\nstate_dir = {}\n",
                toml_string(&directory.path().join("export").display().to_string()),
                toml_string(&directory.path().join("state").display().to_string()),
            ),
        );
        assert_eq!(
            resolve_state_dir(None, Some(&path)).unwrap(),
            directory.path().join("state")
        );
        let explicit = directory.path().join("elsewhere");
        assert_eq!(
            resolve_state_dir(Some(&explicit), Some(&path)).unwrap(),
            explicit
        );
    }

    #[test]
    fn the_packaged_example_validates_against_a_temporary_export() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .unwrap()
            .join("packaging/quickfs.toml.example");
        let text = std::fs::read_to_string(&example)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", example.display()));

        let directory = tempfile::tempdir().unwrap();
        let export = directory.path().join("export");
        let state = directory.path().join("state");
        std::fs::create_dir(&export).unwrap();

        // Substitute only the two deployment paths; every other line, including
        // every limit and the logging filter, is validated exactly as shipped.
        let substituted: String = text
            .lines()
            .map(|line| {
                if line.starts_with("export_root = ") {
                    format!(
                        "export_root = {}",
                        toml_string(&export.display().to_string())
                    )
                } else if line.starts_with("state_dir = ") {
                    format!("state_dir = {}", toml_string(&state.display().to_string()))
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.lines().any(|line| line.starts_with("export_root = "))
                && text.lines().any(|line| line.starts_with("state_dir = ")),
            "the example no longer has the `export_root`/`state_dir` lines this test substitutes"
        );

        let path = write(directory.path(), &substituted);
        let resolved = load_and_resolve(&args_with_config(&path)).unwrap();
        validate(&resolved, false).unwrap();
        assert!(
            !resolved.allow_writes,
            "the packaged example must keep writes disabled"
        );
        assert_eq!(resolved.log_filter.as_deref(), Some("info"));
    }

    #[test]
    fn the_documented_environment_variables_are_wired_to_their_options() {
        use clap::CommandFactory;

        // Clap resolves an explicit `--flag` above its environment variable
        // before `resolve` sees the value, so the precedence chain's first two
        // levels are Clap's. What this asserts is that the security-relevant
        // options are actually connected to the documented variables - a
        // missing `env` would silently drop a deployment's configuration.
        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ServeArgs,
        }
        let command = Wrapper::command();
        for (id, variable) in [
            ("config", "QUICKFS_CONFIG"),
            ("bind", "QUICKFS_BIND"),
            ("export_root", "QUICKFS_EXPORT_ROOT"),
            ("state_dir", "QUICKFS_STATE_DIR"),
            ("allow_writes", "QUICKFS_ALLOW_WRITES"),
        ] {
            let argument = command
                .get_arguments()
                .find(|argument| argument.get_id() == id)
                .unwrap_or_else(|| panic!("no `{id}` option"));
            assert_eq!(
                argument.get_env().and_then(|value| value.to_str()),
                Some(variable),
                "`{id}` is not wired to {variable}"
            );
            assert!(
                argument.get_default_values().is_empty(),
                "`{id}` has a Clap default, which would outrank the TOML value"
            );
        }
    }

    #[test]
    fn allow_writes_is_a_bare_flag_that_can_also_be_turned_off() {
        use clap::Parser;

        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: ServeArgs,
        }
        assert_eq!(
            Wrapper::try_parse_from(["x"]).unwrap().args.allow_writes,
            None
        );
        assert_eq!(
            Wrapper::try_parse_from(["x", "--allow-writes"])
                .unwrap()
                .args
                .allow_writes,
            Some(true)
        );
        assert_eq!(
            Wrapper::try_parse_from(["x", "--allow-writes=false"])
                .unwrap()
                .args
                .allow_writes,
            Some(false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_group_writable_configuration_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = write(
            directory.path(),
            "[server]\nexport_root = \"/srv/export\"\n",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660)).unwrap();
        let error = load(&path).unwrap_err().to_string();
        assert!(error.contains("writable by group"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_configuration_file_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let real = write(
            directory.path(),
            "[server]\nexport_root = \"/srv/export\"\n",
        );
        let link = directory.path().join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let error = load(&link).unwrap_err().to_string();
        assert!(error.contains("symbolic link"), "{error}");
    }
}
