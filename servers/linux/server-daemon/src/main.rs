// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use anyhow::Result;
use clap::{Parser, Subcommand};
use quickfs_common::{DEFAULT_MAX_READ_SIZE, Limits, init_logging};
use quickfs_protocol::*;
use quickfs_server_core::Export;
use quickfs_transport_quic::{
    RecvStream, SendStream, load_certificate, load_private_key, read_frame, server_endpoint,
    write_frame,
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};
use tokio::sync::Semaphore;

#[derive(Parser)]
#[command(name = "server-daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Serve(Serve),
}
#[derive(clap::Args, Clone)]
struct Serve {
    #[arg(long, env = "QUICKFS_BIND", default_value = "0.0.0.0:4433")]
    bind: SocketAddr,
    #[arg(long, env = "QUICKFS_EXPORT_ROOT")]
    export_root: PathBuf,
    #[arg(long, env = "QUICKFS_CERT")]
    cert: PathBuf,
    #[arg(long, env = "QUICKFS_KEY")]
    key: PathBuf,
    #[arg(long, env = "QUICKFS_TOKEN")]
    token: String,
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
    }
}
async fn serve(c: Serve) -> Result<()> {
    let cert = load_certificate(&c.cert)?;
    let key = load_private_key(&c.key)?;
    let endpoint = server_endpoint(c.bind, cert, key)?;
    let export = Arc::new(
        Export::new(
            &c.export_root,
            Limits {
                max_read_size: c.max_read_size,
                max_open_handles: c.max_open_handles,
                request_timeout_ms: c.request_timeout_ms,
            },
        )
        .await?,
    );
    let token = Arc::new(c.token);
    let permits = Arc::new(Semaphore::new(c.max_concurrent_requests));
    tracing::info!(address=%endpoint.local_addr()?,root=%c.export_root.display(),"server listening");
    loop {
        tokio::select! {
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let export = export.clone();
                let token = token.clone();
                let permits = permits.clone();
                tokio::spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            let authenticated = Arc::new(tokio::sync::RwLock::new(false));
                            while let Ok((send, recv)) = connection.accept_bi().await {
                                let Ok(permit) = permits.clone().acquire_owned().await else { break };
                                let export = export.clone();
                                let token = token.clone();
                                let authenticated = authenticated.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    if let Err(error) = handle(send, recv, export, &token, authenticated).await {
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
async fn handle(
    mut send: SendStream,
    mut recv: RecvStream,
    export: Arc<Export>,
    token: &str,
    authenticated: Arc<tokio::sync::RwLock<bool>>,
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
        Request::Hello { .. } | Request::Authenticate { .. } | Request::Ping { .. }
    ) || *authenticated.read().await;
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
        Request::Authenticate { token: provided } => {
            if provided == token {
                *authenticated.write().await = true;
                Response::AuthenticateAck
            } else {
                Response::Error(ProtocolError {
                    code: ErrorCode::Unauthenticated,
                    message: "invalid token".into(),
                })
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
