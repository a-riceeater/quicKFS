//! Delay-load the WinFsp DLL.
//!
//! `quickfs.exe` links against `winfsp-x64.dll`, which lives in the WinFsp
//! installation directory and is not on the default search path. Without delay
//! loading, Windows resolves the import at process start and the binary simply
//! fails to launch on a machine without WinFsp — with a loader error that says
//! nothing about which dependency is missing. Delaying it lets the process
//! start, run `require_winfsp`, and tell the user where to get it.
//!
//! The DLL is still loaded before any WinFsp call: the mount path calls
//! `winfsp_wrs::init`, which locates the installation through the registry and
//! loads it explicitly.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let library = match std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("x86_64") => "winfsp-x64",
        Ok("aarch64") => "winfsp-a64",
        Ok("x86") => "winfsp-x86",
        _ => return,
    };
    println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{library}.dll");
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}
