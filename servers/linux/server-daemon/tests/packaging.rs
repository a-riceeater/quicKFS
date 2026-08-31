// SPDX-License-Identifier: Apache-2.0
//! Runs the packaging shell test suite from `cargo test`.
//!
//! The suite covers the installer and uninstaller: archive verification,
//! hostile archive layouts, the `DESTDIR` layout and modes, upgrade
//! preservation, uninstall preservation, and the static guarantees about what
//! the scripts may never do. It needs a POSIX shell plus `tar` and `sha256sum`,
//! so it runs on Linux - the platform the server daemon and its packaging
//! target - and reports itself skipped elsewhere.
//!
//! Nothing in it requires root, writes outside a temporary directory, creates a
//! user or group, or contacts systemd.

#[cfg(target_os = "linux")]
#[test]
fn packaging_scripts_pass_their_shell_suite() {
    use std::path::Path;
    use std::process::Command;

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    // servers/linux/server-daemon -> servers/linux -> servers -> repository root
    let root = manifest
        .ancestors()
        .nth(3)
        .expect("the daemon manifest should sit three directories below the repository root");
    let suite = root.join("packaging/tests/run.sh");
    assert!(suite.is_file(), "missing {}", suite.display());

    let output = Command::new("sh")
        .arg(&suite)
        .arg(root)
        .output()
        .expect("failed to run the packaging test suite");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    if !output.status.success() {
        panic!("packaging test suite failed\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn packaging_scripts_pass_their_shell_suite() {
    eprintln!(
        "skipped: the packaging suite targets Linux, the server daemon's deployment platform"
    );
}
