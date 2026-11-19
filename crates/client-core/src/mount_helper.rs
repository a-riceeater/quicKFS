// SPDX-License-Identifier: Apache-2.0
//! The launcher-facing half of a mount helper, shared by `quickfs-mount` on
//! macOS and `quickfs mount` on Windows.
//!
//! An app that starts a mount without a terminal needs three things from the
//! helper process: a way to hand it the password, a way to ask it for live link
//! statistics, and a guarantee that a mount does not outlive the app. The first
//! two are implemented here once, over the helper's standard input and output,
//! so both platforms speak exactly the same line protocol. Only the third —
//! noticing that the parent has gone — differs by operating system and stays
//! with each platform's helper.

use crate::{LinkStats, RemoteFilesystem};
use std::{
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use zeroize::Zeroizing;

/// Longest accepted stdin password line, so a misbehaving parent cannot make
/// the mount buffer unbounded input before authenticating.
pub const MAX_STDIN_PASSWORD_BYTES: u64 = 4_096;

/// Prefix of a stats reply line, so the launcher can tell replies apart from
/// the banner and log lines sharing standard output.
pub const STATS_REPLY_PREFIX: &str = "quickfs-stats ";

/// Longest stats request line read whole; longer lines are discarded, so a
/// misbehaving parent cannot make the helper buffer unbounded input.
const MAX_REQUEST_LINE_BYTES: u64 = 256;

/// Why a password could not be read from standard input.
#[derive(Debug, thiserror::Error)]
pub enum PasswordInputError {
    #[error("failed to read the password from standard input: {0}")]
    Io(#[from] std::io::Error),
    #[error("the standard-input password exceeds {MAX_STDIN_PASSWORD_BYTES} bytes")]
    TooLong,
    #[error("no password was supplied on standard input")]
    Empty,
    #[error("the standard-input password is not valid UTF-8")]
    NotUtf8,
}

/// Read the password from the first line of `input`, without the line ending.
pub fn read_password_line(input: impl BufRead) -> Result<Zeroizing<String>, PasswordInputError> {
    let mut line = Zeroizing::new(Vec::new());
    input
        .take(MAX_STDIN_PASSWORD_BYTES + 1)
        .read_until(b'\n', &mut line)?;
    if u64::try_from(line.len()).unwrap_or(u64::MAX) > MAX_STDIN_PASSWORD_BYTES {
        return Err(PasswordInputError::TooLong);
    }
    while matches!(line.last(), Some(b'\n' | b'\r')) {
        line.pop();
    }
    if line.is_empty() {
        return Err(PasswordInputError::Empty);
    }
    let text = std::str::from_utf8(&line).map_err(|_| PasswordInputError::NotUtf8)?;
    Ok(Zeroizing::new(text.to_owned()))
}

/// Milliseconds since the Unix epoch, or zero if the clock is before it.
pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0)
}

/// The JSON document a stats file holds and a stats reply carries.
pub fn stats_snapshot(started_ms: u128, link: Option<LinkStats>) -> serde_json::Value {
    serde_json::json!({
        "pid": std::process::id(),
        "started_ms": started_ms,
        "sampled_ms": unix_millis(),
        "link": link,
    })
}

/// Reply to each `stats` request line until input ends or output fails.
///
/// Each reply is one `write_all` of a whole line, so it never interleaves with
/// the other writers of standard output. Any other line is ignored.
pub fn serve_stats_requests(
    mut input: impl BufRead,
    mut output: impl Write,
    snapshot: impl Fn() -> serde_json::Value,
) {
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

fn skip_line(input: &mut impl BufRead) -> std::io::Result<()> {
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

/// Answer `stats` requests on standard input from a plain thread.
///
/// Standard input blocks, and `link_stats` never waits, so neither touches the
/// runtime serving filesystem callbacks. Returns the spawn error, if any, so
/// the caller can report that statistics are unavailable.
pub fn spawn_stats_responder(filesystem: Arc<dyn RemoteFilesystem>) -> std::io::Result<()> {
    let started = unix_millis();
    std::thread::Builder::new()
        .name("quickfs-stats".into())
        .spawn(move || {
            serve_stats_requests(std::io::stdin().lock(), std::io::stdout(), || {
                stats_snapshot(started, filesystem.link_stats())
            });
        })
        .map(|_| ())
}

/// Rewrite `path` once a second with a stats snapshot. Failures are ignored:
/// telemetry must never disturb the mount.
pub fn spawn_stats_writer(
    runtime: &tokio::runtime::Handle,
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

/// Write `bytes` to a private temporary file beside `path` and rename it into
/// place, so a reader never sees a partial document.
pub fn write_private_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = private_file(&temporary)?;
    file.write_all(bytes)?;
    drop(file);
    std::fs::rename(&temporary, path)
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    // The owner-only protected DACL is the Windows counterpart of mode 0600.
    quickfs_windows_support::harden_private_path(path)?;
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn private_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::create(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn stdin_password_strips_line_endings_and_rejects_empty_or_huge_input() {
        assert_eq!(
            read_password_line(&b"secret\r\nignored"[..])
                .unwrap()
                .as_str(),
            "secret"
        );
        assert!(matches!(
            read_password_line(&b"\n"[..]),
            Err(PasswordInputError::Empty)
        ));
        assert!(matches!(
            read_password_line(&vec![b'a'; 5_000][..]),
            Err(PasswordInputError::TooLong)
        ));
        assert!(matches!(
            read_password_line(&b"\xff\xfe\n"[..]),
            Err(PasswordInputError::NotUtf8)
        ));
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
        // The over-long line swallows the "stats" glued to its end; the final
        // unterminated request is incomplete and not answered.
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "quickfs-stats {\"n\":1}\nquickfs-stats {\"n\":2}\n"
        );
    }

    #[test]
    fn a_stats_file_is_replaced_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stats.json");
        write_private_atomically(&path, b"{\"a\":1}").unwrap();
        write_private_atomically(&path, b"{\"b\":2}").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{\"b\":2}");
        assert!(!path.with_extension("tmp").exists());
    }
}
