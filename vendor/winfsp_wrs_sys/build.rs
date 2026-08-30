//! Locate the WinFsp import library.
//!
//! Resolution order, first hit wins:
//!
//! 1. `WINFSP_LIB_DIR` — the directory that directly contains `winfsp-*.lib`.
//! 2. `WINFSP_INSTALL_DIR` — a WinFsp SDK/installation root; `lib/` beneath it.
//! 3. `HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir` — a machine-wide install.
//!
//! Upstream only implements (3) and panics otherwise, which makes an
//! unprivileged or unpacked-SDK build impossible. The link *search* path is all
//! that changes; the symbols, the calling convention, and the DLL that
//! satisfies them at run time are identical, so a binary built through (1) or
//! (2) still requires a real WinFsp installation to mount anything.

use std::path::PathBuf;

#[cfg(windows)]
fn registry_install_dir() -> Option<PathBuf> {
    let value = registry::Hive::LocalMachine
        .open(r"SOFTWARE\WOW6432Node\WinFsp", registry::Security::Read)
        .ok()?
        .value("InstallDir")
        .ok()?;
    match value {
        registry::Data::String(path) => Some(PathBuf::from(path.to_os_string())),
        _ => None,
    }
}

#[cfg(not(windows))]
fn registry_install_dir() -> Option<PathBuf> {
    None
}

fn library_directory() -> PathBuf {
    println!("cargo:rerun-if-env-changed=WINFSP_LIB_DIR");
    println!("cargo:rerun-if-env-changed=WINFSP_INSTALL_DIR");

    if let Some(directory) = std::env::var_os("WINFSP_LIB_DIR") {
        return PathBuf::from(directory);
    }
    if let Some(directory) = std::env::var_os("WINFSP_INSTALL_DIR") {
        return PathBuf::from(directory).join("lib");
    }
    match registry_install_dir() {
        Some(directory) => directory.join("lib"),
        None => panic!(
            "WinFsp was not found. Install WinFsp (https://winfsp.dev/) or point \
             WINFSP_INSTALL_DIR at an unpacked SDK root (the directory containing \
             inc/ and lib/), or WINFSP_LIB_DIR directly at the directory holding \
             winfsp-x64.lib."
        ),
    }
}

#[cfg(windows)]
fn main() {
    let directory = library_directory();
    let architecture = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let environment = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let library = match (architecture.as_str(), environment.as_str()) {
        ("x86_64", "msvc") => "winfsp-x64",
        ("x86", "msvc") => "winfsp-x86",
        ("aarch64", "msvc") => "winfsp-a64",
        _ => panic!(
            "unsupported target {}; WinFsp ships import libraries only for the MSVC \
             x86_64, x86 and aarch64 Windows targets",
            std::env::var("TARGET").unwrap_or_default()
        ),
    };

    let import_library = directory.join(format!("{library}.lib"));
    if !import_library.is_file() {
        panic!(
            "WinFsp import library '{}' does not exist. Check WINFSP_LIB_DIR / \
             WINFSP_INSTALL_DIR or reinstall WinFsp.",
            import_library.display()
        );
    }
    println!("cargo:rustc-link-search={}", directory.display());
    println!("cargo:rustc-link-lib=dylib={library}");
}

// Compilation on a non-Windows platform obviously cannot link WinFsp, but
// keeping the Rust side platform agnostic lets linters and IDEs work when
// reading this code from another operating system.
#[cfg(not(windows))]
fn main() {}
