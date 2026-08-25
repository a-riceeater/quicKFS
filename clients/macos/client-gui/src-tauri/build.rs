// SPDX-License-Identifier: Apache-2.0

fn main() {
    // The `tauri` runtime crate — whose build handshake `tauri_build::build`
    // verifies — is a macOS-only dependency of this crate, so running the
    // Tauri build step for any other target panics (`missing cargo:dev
    // instruction`) and has broken every non-macOS build of the workspace.
    // Other targets compile the plain stub `main` and need no Tauri codegen.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        tauri_build::build();
    }
}
