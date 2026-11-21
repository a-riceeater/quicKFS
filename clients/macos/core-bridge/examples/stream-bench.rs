// SPDX-License-Identifier: Apache-2.0
//! Read a remote file through the bridge's streaming API the way a video
//! player does, and report how long each read waited. Reads run one at a
//! time, like one player request; `--rate` paces them to a bitrate so stalls
//! show up as they would on screen rather than averaging away.
//!
//! ```sh
//! cargo run --release -p quickfs-core-bridge --example stream-bench -- \
//!   --state <dir> --server 10.0.0.74:4433 --name RAID --user chat \
//!   --path "/clip.mp4" --rate 10 --seconds 30 [--from 0] [--chunk 1]
//! ```
//! The password comes from `QUICKFS_PASSWORD`. `--save <file>` keeps the
//! bytes read, for example to probe a file's header.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn call(request: Value) -> Value {
    let reply: Value = serde_json::from_str(&quickfs_core_bridge::call_json(&request.to_string()))
        .unwrap_or_else(|error| panic!("unreadable reply: {error}"));
    if let Some(error) = reply.get("error") {
        panic!("{request}: {error}");
    }
    reply["ok"].clone()
}

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|index| args.get(index + 1).cloned())
}

fn main() {
    let state = arg("--state").expect("--state");
    let server = arg("--server").expect("--server");
    let name = arg("--name").unwrap_or_else(|| "localhost".into());
    let user = arg("--user").expect("--user");
    let path = arg("--path").expect("--path");
    let password = std::env::var("QUICKFS_PASSWORD").expect("QUICKFS_PASSWORD");
    // Megabytes per second the reader consumes; 0 reads flat out.
    let rate: f64 = arg("--rate").map_or(0.0, |v| v.parse().expect("--rate"));
    let seconds: f64 = arg("--seconds").map_or(30.0, |v| v.parse().expect("--seconds"));
    let from: u64 = arg("--from").map_or(0, |v| v.parse::<u64>().expect("--from") << 20);
    let chunk: u64 = arg("--chunk").map_or(1, |v| v.parse().expect("--chunk")) << 20;
    let cache = arg("--cache").unwrap_or_else(|| format!("{state}/cache"));
    // Keep what was read, for inspecting a file's header with other tools.
    let mut save = arg("--save").map(|path| std::fs::File::create(path).expect("--save"));

    let endpoint = json!({ "address": server, "server_name": name });
    let session = call(json!({
        "op": "connect", "state_dir": state, "server": endpoint, "trust": { "kind": "pinned" },
        "username": user, "password": password,
    }))["session"]
        .as_u64()
        .expect("session");
    let node =
        call(json!({ "op": "resolve_path", "session": session, "path": path }))["node"]["node"]
            .as_str()
            .expect("node")
            .to_owned();
    let opened = Instant::now();
    let stream = call(json!({
        "op": "stream_open", "session": session, "node": node, "cache_dir": cache,
    }));
    let (id, size) = (
        stream["stream"].as_u64().expect("stream"),
        stream["size"].as_u64().expect("size"),
    );
    println!("opened {size} B in {:?}", opened.elapsed());

    let start = Instant::now();
    let mut offset = from;
    let mut read_total = 0_u64;
    let mut waits: Vec<Duration> = Vec::new();
    let mut second_mark = Instant::now();
    let mut second_bytes = 0_u64;
    while start.elapsed().as_secs_f64() < seconds && offset < size {
        let began = Instant::now();
        let data = quickfs_core_bridge::stream_read(id, offset, chunk).expect("read");
        let waited = began.elapsed();
        waits.push(waited);
        if waited > Duration::from_millis(100) {
            println!("  slow read at {} MiB: {:?}", offset >> 20, waited);
        }
        if let Some(file) = save.as_mut() {
            std::io::Write::write_all(file, &data).expect("save");
        }
        offset += data.len() as u64;
        read_total += data.len() as u64;
        second_bytes += data.len() as u64;
        if second_mark.elapsed() >= Duration::from_secs(1) {
            println!(
                "{:5.1} s  {:6.1} MB/s",
                start.elapsed().as_secs_f64(),
                second_bytes as f64 / second_mark.elapsed().as_secs_f64() / 1e6
            );
            second_mark = Instant::now();
            second_bytes = 0;
        }
        if rate > 0.0 {
            // Consume at the bitrate: the next read is due when the player
            // would have played through what it has.
            let due = start + Duration::from_secs_f64(read_total as f64 / (rate * 1e6));
            if let Some(sleep) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(sleep);
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64();
    waits.sort();
    let pick = |q: f64| waits[((waits.len() - 1) as f64 * q) as usize];
    println!(
        "read {:.0} MB in {elapsed:.1} s = {:.1} MB/s; reads {} p50 {:?} p99 {:?} max {:?}; >100 ms: {}",
        read_total as f64 / 1e6,
        read_total as f64 / elapsed / 1e6,
        waits.len(),
        pick(0.5),
        pick(0.99),
        pick(1.0),
        waits
            .iter()
            .filter(|w| **w > Duration::from_millis(100))
            .count()
    );
}
