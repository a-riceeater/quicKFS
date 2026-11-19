// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]
//! The remote file commands shared by `quickfs-client-cli` on macOS and
//! `quickfs remote` on Windows.
//!
//! These exercise an export without mounting it: list, stat, read and write
//! byte ranges, create, truncate, rename and remove. Both command lines take
//! the definitions and the implementation from here, so a command behaves the
//! same way on both platforms and is tested once.

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use quickfs_client_core::{MAX_CLIENT_WRITE_SIZE, RemoteFilesystem, mount_helper, resolve_path};
use quickfs_protocol::{AttributeChanges, FileAccess, FileOpenOptions, NodeId, RenameMode};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

/// A command run against an authenticated export.
#[derive(Clone, Debug, Subcommand)]
pub enum RemoteCommand {
    /// Measure that the server answers.
    Ping,
    /// Show what the export allows.
    Capabilities,
    /// Show the export's size and free space.
    FilesystemStats,
    /// List a directory.
    List { path: String },
    /// Show a node's metadata.
    Stat { path: String },
    /// Write a byte range of a remote file to standard output.
    Read {
        path: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        length: u64,
    },
    /// Write a local file into an existing remote file at an arbitrary byte offset.
    Write {
        path: String,
        input: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long, default_value_t = false)]
        sync: bool,
    },
    /// Atomically create a new empty remote file.
    Create {
        path: String,
        #[arg(long, default_value_t = 0o644)]
        mode: u32,
    },
    /// Set a remote file's length.
    Truncate { path: String, size: u64 },
    /// Create a remote directory.
    Mkdir {
        path: String,
        #[arg(long, default_value_t = 0o755)]
        mode: u32,
    },
    /// Remove a remote file, or an empty directory with `--directory`.
    Remove {
        path: String,
        #[arg(long, default_value_t = false)]
        directory: bool,
    },
    /// Rename or move a remote entry.
    Rename {
        source: String,
        destination: String,
        #[arg(long, default_value_t = false)]
        no_replace: bool,
    },
}

/// Read the account password: from the first line of standard input when
/// `from_stdin` is set (for scripts and apps), otherwise from the console.
pub fn read_password(from_stdin: bool) -> Result<Zeroizing<String>> {
    if from_stdin {
        Ok(mount_helper::read_password_line(std::io::stdin().lock())?)
    } else {
        Ok(Zeroizing::new(rpassword::prompt_password("Password: ")?))
    }
}

/// Run one command, writing its human-readable result to `output`.
pub async fn run(
    filesystem: &dyn RemoteFilesystem,
    command: &RemoteCommand,
    output: &mut dyn Write,
) -> Result<()> {
    match command {
        RemoteCommand::Ping => writeln!(output, "pong {}", filesystem.ping(42).await?)?,
        RemoteCommand::Capabilities => writeln!(output, "{:#?}", filesystem.capabilities().await?)?,
        RemoteCommand::FilesystemStats => {
            writeln!(output, "{:#?}", filesystem.stat_filesystem().await?)?
        }
        RemoteCommand::List { path } => {
            let node = resolve_path(filesystem, path).await?;
            for entry in filesystem.list_directory(node).await? {
                writeln!(
                    output,
                    "{:?}\t{}",
                    entry.kind,
                    String::from_utf8_lossy(entry.name.as_bytes())
                )?;
            }
        }
        RemoteCommand::Stat { path } => {
            let node = resolve_path(filesystem, path).await?;
            writeln!(output, "{:#?}", filesystem.get_metadata(node).await?)?;
        }
        RemoteCommand::Read {
            path,
            offset,
            length,
        } => {
            let node = resolve_path(filesystem, path).await?;
            let (handle, _, _) = filesystem.open_file(node).await?;
            let result = filesystem.read_range(handle, *offset, *length).await;
            filesystem
                .close_file(handle)
                .await
                .context("closing file")?;
            output.write_all(&result?)?;
        }
        RemoteCommand::Write {
            path,
            input,
            offset,
            sync,
        } => {
            let node = resolve_path(filesystem, path).await?;
            let opened = filesystem
                .open_file_with_options(
                    node,
                    FileOpenOptions {
                        access: FileAccess::ReadWrite,
                        truncate: false,
                        append: false,
                    },
                )
                .await?;
            let result = write_input(filesystem, opened.handle, input, *offset, *sync).await;
            let close_result = filesystem.close_file(opened.handle).await;
            let written = result?;
            close_result.context("closing file after write")?;
            writeln!(output, "wrote {written} bytes at offset {offset}")?;
        }
        RemoteCommand::Create { path, mode } => {
            let (parent, name) = resolve_parent(filesystem, path).await?;
            let created = filesystem
                .create_file(
                    parent,
                    name.into(),
                    *mode,
                    FileOpenOptions {
                        access: FileAccess::ReadWrite,
                        truncate: false,
                        append: false,
                    },
                )
                .await?;
            filesystem.sync_file(created.opened.handle, false).await?;
            filesystem.close_file(created.opened.handle).await?;
            filesystem.sync_directory(parent).await?;
            writeln!(output, "created file {:?}", created.metadata.node)?;
        }
        RemoteCommand::Truncate { path, size } => {
            let node = resolve_path(filesystem, path).await?;
            let metadata = filesystem
                .set_attributes(
                    node,
                    None,
                    AttributeChanges {
                        size: Some(*size),
                        mode: None,
                        accessed_unix_ms: None,
                        modified_unix_ms: None,
                        backup_unix_ms: None,
                    },
                )
                .await?;
            writeln!(
                output,
                "truncated to {} bytes (revision {})",
                metadata.size, metadata.revision
            )?;
        }
        RemoteCommand::Mkdir { path, mode } => {
            let (parent, name) = resolve_parent(filesystem, path).await?;
            let metadata = filesystem
                .create_directory(parent, name.into(), *mode)
                .await?;
            writeln!(output, "created directory {:?}", metadata.node)?;
        }
        RemoteCommand::Remove { path, directory } => {
            let (parent, name) = resolve_parent(filesystem, path).await?;
            filesystem
                .remove_node(parent, name.into(), *directory)
                .await?;
        }
        RemoteCommand::Rename {
            source,
            destination,
            no_replace,
        } => {
            let (parent, name) = resolve_parent(filesystem, source).await?;
            let (new_parent, new_name) = resolve_parent(filesystem, destination).await?;
            filesystem
                .rename_node(
                    parent,
                    name.into(),
                    new_parent,
                    new_name.into(),
                    if *no_replace {
                        RenameMode::NoReplace
                    } else {
                        RenameMode::Replace
                    },
                )
                .await?;
            filesystem.sync_directory(parent).await?;
            if new_parent != parent {
                filesystem.sync_directory(new_parent).await?;
            }
        }
    }
    Ok(())
}

async fn write_input(
    filesystem: &dyn RemoteFilesystem,
    handle: quickfs_protocol::FileHandle,
    input: &Path,
    offset: u64,
    sync: bool,
) -> Result<u64> {
    let capacity = usize::try_from(MAX_CLIENT_WRITE_SIZE)
        .context("client write limit does not fit this platform")?;
    let mut file = fs::File::open(input)
        .with_context(|| format!("failed to open input '{}'", input.display()))?;
    let mut buffer = vec![0; capacity];
    let mut written = 0_u64;
    loop {
        let amount = file.read(&mut buffer)?;
        if amount == 0 {
            break;
        }
        let position = offset
            .checked_add(written)
            .context("write offset overflows")?;
        let result = filesystem
            .write_range(handle, position, &buffer[..amount])
            .await?;
        if result.written != amount as u64 {
            bail!("server reported a partial write");
        }
        written = written
            .checked_add(result.written)
            .context("written byte count overflows")?;
    }
    filesystem.flush_file(handle, None).await?;
    if sync {
        filesystem.sync_file(handle, false).await?;
    }
    Ok(written)
}

/// Split a remote path into its resolved parent directory and final name.
pub async fn resolve_parent(
    filesystem: &dyn RemoteFilesystem,
    path: &str,
) -> Result<(NodeId, String)> {
    let (parent_path, name) = split_parent_path(path)?;
    Ok((
        resolve_path(filesystem, parent_path).await?,
        name.to_owned(),
    ))
}

/// The pure half of [`resolve_parent`]: which parent path and which name.
fn split_parent_path(path: &str) -> Result<(&str, &str)> {
    let trimmed = path.trim_end_matches('/');
    let (parent_path, name) = match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/", name),
        Some((parent, name)) => (parent, name),
        None => ("/", trimmed),
    };
    if name.is_empty() || name == "." || name == ".." {
        bail!("path must end in a normal entry name");
    }
    Ok((parent_path, name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_path_splits_into_parent_and_name() {
        assert_eq!(split_parent_path("/a/b/c").unwrap(), ("/a/b", "c"));
        assert_eq!(split_parent_path("/top").unwrap(), ("/", "top"));
        assert_eq!(split_parent_path("bare").unwrap(), ("/", "bare"));
        assert_eq!(split_parent_path("/dir/").unwrap(), ("/", "dir"));
        assert!(split_parent_path("/").is_err());
        assert!(split_parent_path("/a/..").is_err());
    }

    #[test]
    fn the_commands_parse_with_their_documented_arguments() {
        use clap::Parser;
        #[derive(Parser)]
        struct Probe {
            #[command(subcommand)]
            command: RemoteCommand,
        }
        let parsed = Probe::try_parse_from(["probe", "read", "/x", "--length", "8"]).unwrap();
        assert!(matches!(
            parsed.command,
            RemoteCommand::Read {
                offset: 0,
                length: 8,
                ..
            }
        ));
        assert!(Probe::try_parse_from(["probe", "read", "/x"]).is_err());
        let parsed =
            Probe::try_parse_from(["probe", "rename", "/a", "/b", "--no-replace"]).unwrap();
        assert!(matches!(
            parsed.command,
            RemoteCommand::Rename {
                no_replace: true,
                ..
            }
        ));
    }
}
