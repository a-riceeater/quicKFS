// SPDX-License-Identifier: Apache-2.0
//! Send bridge requests from a terminal: one JSON request per input line,
//! one JSON reply per output line. Useful for exercising the exact code path
//! the macOS app uses without building the app.
//!
//! ```sh
//! echo '{"op":"environment"}' | cargo run -p quickfs-core-bridge --example bridge-repl
//! ```

use std::io::{BufRead, Write};

fn main() -> std::io::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        writeln!(out, "{}", quickfs_core_bridge::call_json(&line))?;
        out.flush()?;
    }
    Ok(())
}
