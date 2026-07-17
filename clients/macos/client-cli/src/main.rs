// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use quickfs_client_core::{NetworkFilesystem, RemoteFilesystem, resolve_path};
use quickfs_transport_quic::{QuicClient, load_certificate};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
#[derive(Parser)]
#[command(name = "client-cli")]
struct Cli {
    #[arg(long, env = "QUICKFS_SERVER", default_value = "127.0.0.1:4433")]
    server: SocketAddr,
    #[arg(long, default_value = "localhost")]
    server_name: String,
    #[arg(long, env = "QUICKFS_CERT")]
    cert: PathBuf,
    #[arg(long, env = "QUICKFS_TOKEN")]
    token: String,
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Ping,
    List {
        path: String,
    },
    Stat {
        path: String,
    },
    Read {
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: u64,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    let c = Cli::parse();
    let cert = load_certificate(&c.cert)?;
    let transport = QuicClient::connect(
        c.server,
        &c.server_name,
        cert,
        Duration::from_millis(c.timeout_ms),
    )
    .await?;
    let fs = NetworkFilesystem::authenticate(transport, c.token).await?;
    match c.command {
        Command::Ping => println!("pong {}", fs.ping(42).await?),
        Command::List { path } => {
            let node = resolve_path(&fs, &path).await?;
            for e in fs.list_directory(node).await? {
                println!("{:?}\t{}", e.kind, e.name)
            }
        }
        Command::Stat { path } => {
            let node = resolve_path(&fs, &path).await?;
            println!("{:#?}", fs.get_metadata(node).await?)
        }
        Command::Read {
            path,
            offset,
            length,
        } => {
            let node = resolve_path(&fs, &path).await?;
            let (h, _, _) = fs.open_file(node).await?;
            let result = fs.read_range(h, offset, length).await;
            fs.close_file(h).await.context("closing file")?;
            let bytes = result?;
            use std::io::Write;
            std::io::stdout().write_all(&bytes)?
        }
    }
    Ok(())
}
