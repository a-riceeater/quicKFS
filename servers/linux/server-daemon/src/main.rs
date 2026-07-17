// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use quickfs_auth::{
    StatePaths, add_user, certificate_fingerprint, change_password, consume_pairing,
    create_pairing, initialize, load_pairing, remove_user, set_user_enabled, verify_user,
};
use quickfs_common::{DEFAULT_MAX_READ_SIZE, Limits, init_logging};
use quickfs_protocol::*;
use quickfs_server_core::Export;
use quickfs_transport_quic::{
    RecvStream, SendStream, load_certificate, load_private_key, read_frame, server_endpoint,
    write_frame,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use zeroize::Zeroize;

#[derive(Parser)]
#[command(name = "server-daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve(Serve),
    /// Create persistent server identity and authentication state.
    Init(Init),
    /// Manage user accounts.
    User(UserCommand),
    /// Manage one-time client pairing sessions.
    Pair(PairCommand),
}

#[derive(clap::Args)]
struct Init {
    #[arg(long, default_value = ".quickfs")]
    state_dir: PathBuf,
    #[arg(long = "server-name", required = true)]
    server_names: Vec<String>,
}

#[derive(clap::Args)]
struct UserCommand {
    #[command(subcommand)]
    command: UserSubcommand,
}

#[derive(Subcommand)]
enum UserSubcommand {
    Add {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Password {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Enable {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Disable {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
    Delete {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        username: String,
    },
}

#[derive(clap::Args)]
struct PairCommand {
    #[command(subcommand)]
    command: PairSubcommand,
}

#[derive(Subcommand)]
enum PairSubcommand {
    Create {
        #[arg(long, default_value = ".quickfs")]
        state_dir: PathBuf,
        #[arg(long, default_value_t = 300)]
        expires_seconds: u64,
    },
}
#[derive(clap::Args, Clone)]
struct Serve {
    #[arg(long, env = "QUICKFS_BIND", default_value = "0.0.0.0:4433")]
    bind: SocketAddr,
    #[arg(long, env = "QUICKFS_EXPORT_ROOT")]
    export_root: PathBuf,
    #[arg(long, env = "QUICKFS_STATE_DIR", default_value = ".quickfs")]
    state_dir: PathBuf,
    #[arg(long,default_value_t=DEFAULT_MAX_READ_SIZE)]
    max_read_size: u64,
    #[arg(long, default_value_t = 1024)]
    max_open_handles: usize,
    #[arg(long, default_value_t = 30_000)]
    request_timeout_ms: u64,
    #[arg(long, default_value_t = 128)]
    max_concurrent_requests: usize,
}
#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let Cli { command } = Cli::parse();
    match command {
        Command::Serve(c) => serve(c).await,
        Command::Init(c) => init(c),
        Command::User(c) => manage_user(c),
        Command::Pair(c) => manage_pairing(c),
    }
}

fn init(command: Init) -> Result<()> {
    let paths = initialize(&command.state_dir, command.server_names)
        .with_context(|| format!("failed to initialize '{}'", command.state_dir.display()))?;
    println!("Initialized quicKFS server state:");
    println!("  certificate: {}", paths.certificate.display());
    println!("  private key: {}", paths.private_key.display());
    println!("Next, add a user with `server-daemon user add <USERNAME>`.");
    Ok(())
}

fn manage_user(command: UserCommand) -> Result<()> {
    match command.command {
        UserSubcommand::Add {
            state_dir,
            username,
        } => {
            let mut password = prompt_new_password()?;
            add_user(&state_dir, &username, password.as_bytes())
                .with_context(|| format!("failed to add user '{username}'"))?;
            password.zeroize();
            println!("Added user '{username}'.");
            Ok(())
        }
        UserSubcommand::Password {
            state_dir,
            username,
        } => {
            let mut password = prompt_new_password()?;
            change_password(&state_dir, &username, password.as_bytes())
                .with_context(|| format!("failed to change password for '{username}'"))?;
            password.zeroize();
            println!("Changed password for '{username}'.");
            Ok(())
        }
        UserSubcommand::Enable {
            state_dir,
            username,
        } => {
            set_user_enabled(&state_dir, &username, true)?;
            println!("Enabled user '{username}'.");
            Ok(())
        }
        UserSubcommand::Disable {
            state_dir,
            username,
        } => {
            set_user_enabled(&state_dir, &username, false)?;
            println!(
                "Disabled user '{username}'. Existing authenticated connections are not revoked."
            );
            Ok(())
        }
        UserSubcommand::Delete {
            state_dir,
            username,
        } => {
            remove_user(&state_dir, &username)?;
            println!(
                "Deleted user '{username}'. Existing authenticated connections are not revoked."
            );
            Ok(())
        }
    }
}

fn prompt_new_password() -> Result<String> {
    let mut password = rpassword::prompt_password("Password: ")?;
    let mut confirmation = rpassword::prompt_password("Confirm password: ")?;
    if password != confirmation {
        password.zeroize();
        confirmation.zeroize();
        bail!("passwords do not match");
    }
    confirmation.zeroize();
    Ok(password)
}

fn manage_pairing(command: PairCommand) -> Result<()> {
    match command.command {
        PairSubcommand::Create {
            state_dir,
            expires_seconds,
        } => {
            let pairing = create_pairing(&state_dir, Duration::from_secs(expires_seconds))?;
            println!("Pairing ID: {}", pairing.id);
            println!("Pairing code: {}", pairing.code);
            println!("Expires at Unix time: {}", pairing.expires_unix_seconds);
            println!(
                "The code is single-use. Transfer it to the client through a trusted channel."
            );
            Ok(())
        }
    }
}

async fn serve(c: Serve) -> Result<()> {
    validate_configuration(&c)?;

    let state_paths = StatePaths::new(&c.state_dir);
    let cert = load_certificate(&state_paths.certificate).with_context(|| {
        format!(
            "failed to load server identity '{}'; run `server-daemon init` first",
            state_paths.certificate.display()
        )
    })?;
    let fingerprint = certificate_fingerprint(cert.as_ref());
    let key = load_private_key(&state_paths.private_key).with_context(|| {
        format!(
            "failed to load private key '{}'",
            state_paths.private_key.display()
        )
    })?;
    let export = Arc::new(
        Export::new(
            &c.export_root,
            Limits {
                max_read_size: c.max_read_size,
                max_open_handles: c.max_open_handles,
                request_timeout_ms: c.request_timeout_ms,
            },
        )
        .await
        .with_context(|| {
            format!(
                "failed to open export root '{}'; ensure it exists, is a directory, and is readable",
                c.export_root.display()
            )
        })?,
    );
    let endpoint = server_endpoint(c.bind, cert, key).with_context(|| {
        format!(
            "failed to configure TLS or bind the QUIC server to '{}'",
            c.bind
        )
    })?;
    let auth_root = Arc::new(c.state_dir.clone());
    let permits = Arc::new(Semaphore::new(c.max_concurrent_requests));
    let local_address = endpoint
        .local_addr()
        .context("failed to determine the bound server address")?;
    tracing::info!(address=%local_address,root=%c.export_root.display(),"server listening");
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let export = export.clone();
                let auth_root = auth_root.clone();
                let permits = permits.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            let auth_state = Arc::new(tokio::sync::RwLock::new(ConnectionAuth::default()));
                            while let Ok((send, recv)) = connection.accept_bi().await {
                                let Ok(permit) = permits.clone().acquire_owned().await else { break };
                                let export = export.clone();
                                let auth_root = auth_root.clone();
                                let auth_state = auth_state.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = handle(send, recv, export, auth_root, fingerprint, auth_state).await {
                                        tracing::warn!(%error, "request failed");
                                    }
                                });
                            }
                        }
                        Err(error) => tracing::warn!(%error, "connection failed"),
                    }
                });
            }
            _ = shutdown_signal() => {
                endpoint.close(0u32.into(), b"server shutdown");
                endpoint.wait_idle().await;
                break;
            }
        }
    }
    Ok(())
}

fn validate_configuration(c: &Serve) -> Result<()> {
    if c.max_read_size == 0 {
        bail!("maximum read size must be greater than zero");
    }
    if c.max_open_handles == 0 {
        bail!("maximum open handles must be greater than zero");
    }
    if c.request_timeout_ms == 0 {
        bail!("request timeout must be greater than zero milliseconds");
    }
    if c.max_concurrent_requests == 0 {
        bail!("maximum concurrent requests must be greater than zero");
    }
    Ok(())
}

#[derive(Default)]
struct ConnectionAuth {
    authenticated: bool,
    failed_attempts: u8,
}
async fn handle(
    mut send: SendStream,
    mut recv: RecvStream,
    export: Arc<Export>,
    auth_root: Arc<PathBuf>,
    fingerprint: [u8; 32],
    auth_state: Arc<tokio::sync::RwLock<ConnectionAuth>>,
) -> Result<()> {
    let request: Envelope<Request> = read_frame(&mut recv).await?;
    let id = request.request_id;
    if request.version != PROTOCOL_VERSION {
        write_frame(
            &mut send,
            &Envelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                message: Response::Error(ProtocolError {
                    code: ErrorCode::UnsupportedVersion,
                    message: "unsupported protocol version".into(),
                }),
            },
        )
        .await?;
        return Ok(());
    }
    let allowed = matches!(
        request.message,
        Request::Hello { .. }
            | Request::Pair { .. }
            | Request::Authenticate { .. }
            | Request::Ping { .. }
    ) || auth_state.read().await.authenticated;
    if !allowed {
        write_response(
            &mut send,
            id,
            Response::Error(ProtocolError {
                code: ErrorCode::Unauthenticated,
                message: "authenticate first".into(),
            }),
        )
        .await?;
        return Ok(());
    }
    let mut raw = None;
    let response = match request.message {
        Request::Hello { .. } => Response::HelloAck {
            version: PROTOCOL_VERSION,
        },
        Request::Pair {
            pairing_id,
            client_nonce,
        } => match load_pairing(&auth_root, pairing_id) {
            Ok(secret) => match secret.proof(&fingerprint, &client_nonce) {
                Ok(proof) => match consume_pairing(&auth_root, pairing_id) {
                    Ok(()) => Response::PairingProof {
                        certificate_fingerprint: fingerprint,
                        proof,
                    },
                    Err(error) => Response::Error(ProtocolError {
                        code: ErrorCode::Unauthenticated,
                        message: error.to_string(),
                    }),
                },
                Err(error) => Response::Error(ProtocolError {
                    code: ErrorCode::Internal,
                    message: error.to_string(),
                }),
            },
            Err(error) => Response::Error(ProtocolError {
                code: ErrorCode::Unauthenticated,
                message: error.to_string(),
            }),
        },
        Request::Authenticate {
            username,
            mut password,
        } => {
            let locked = auth_state.read().await.failed_attempts >= 5;
            if locked {
                password.zeroize();
                Response::Error(ProtocolError {
                    code: ErrorCode::Unauthenticated,
                    message: "too many failed authentication attempts; reconnect to try again"
                        .into(),
                })
            } else {
                let root = auth_root.as_ref().clone();
                let log_username = username.clone();
                let mut password_bytes = password.as_bytes().to_vec();
                password.zeroize();
                let verified = tokio::task::spawn_blocking(move || {
                    let result = verify_user(&root, &username, &password_bytes);
                    password_bytes.zeroize();
                    result
                })
                .await;
                match verified {
                    Ok(Ok(true)) => {
                        auth_state.write().await.authenticated = true;
                        tracing::info!(username = %log_username, "user authenticated");
                        Response::AuthenticateAck
                    }
                    Ok(Ok(false)) => {
                        auth_state.write().await.failed_attempts += 1;
                        tracing::warn!(username = %log_username, "authentication failed");
                        Response::Error(ProtocolError {
                            code: ErrorCode::Unauthenticated,
                            message: "invalid username or password".into(),
                        })
                    }
                    Ok(Err(error)) => Response::Error(ProtocolError {
                        code: ErrorCode::Internal,
                        message: error.to_string(),
                    }),
                    Err(error) => Response::Error(ProtocolError {
                        code: ErrorCode::Internal,
                        message: format!("authentication task failed: {error}"),
                    }),
                }
            }
        }
        Request::Ping { nonce } => Response::Pong { nonce },
        Request::GetMetadata { node } => export
            .metadata(node)
            .await
            .map(Response::Metadata)
            .unwrap_or_else(|e| Response::Error(e.protocol())),
        Request::ListDirectory { node } => export
            .list(node)
            .await
            .map(|entries| Response::DirectoryListing {
                revision: 0,
                entries,
            })
            .unwrap_or_else(|e| Response::Error(e.protocol())),
        Request::OpenFile { node } => export
            .open(node)
            .await
            .map(|(handle, revision, size)| Response::FileOpened {
                handle,
                revision,
                size,
            })
            .unwrap_or_else(|e| Response::Error(e.protocol())),
        Request::ReadRange {
            handle,
            offset,
            length,
        } => match export.read(handle, offset, length).await {
            Ok((revision, data)) => {
                let length = data.len() as u64;
                raw = Some(data);
                Response::ReadData { revision, length }
            }
            Err(e) => Response::Error(e.protocol()),
        },
        Request::CloseFile { handle } => export
            .close(handle)
            .map(|()| Response::FileClosed)
            .unwrap_or_else(|e| Response::Error(e.protocol())),
    };
    write_response(&mut send, id, response).await?;
    if let Some(data) = raw {
        send.write_all(&data).await?
    }
    send.finish()?;
    Ok(())
}
async fn write_response(send: &mut SendStream, id: RequestId, message: Response) -> Result<()> {
    write_frame(
        send,
        &Envelope {
            version: PROTOCOL_VERSION,
            request_id: id,
            message,
        },
    )
    .await?;
    Ok(())
}
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}};
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
