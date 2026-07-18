// SPDX-License-Identifier: Apache-2.0
#![forbid(unsafe_code)]

#[cfg(target_os = "macos")]
mod macos {
    use serde::Serialize;

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct FrontendBootstrap {
        macfuse_installed: bool,
        macfuse_install_url: &'static str,
        max_client_read_size: u64,
        platform: &'static str,
    }

    #[tauri::command]
    fn frontend_bootstrap() -> FrontendBootstrap {
        FrontendBootstrap {
            macfuse_installed: quickfs_macos_support::macfuse_is_installed(),
            macfuse_install_url: quickfs_macos_support::MACFUSE_INSTALL_URL,
            max_client_read_size: quickfs_client_core::MAX_CLIENT_READ_SIZE,
            platform: std::env::consts::OS,
        }
    }

    #[tauri::command]
    fn open_macfuse_install_page() -> Result<(), String> {
        let status = std::process::Command::new("/usr/bin/open")
            .arg(quickfs_macos_support::MACFUSE_INSTALL_URL)
            .status()
            .map_err(|error| format!("failed to open the macFUSE website: {error}"))?;

        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "macOS could not open the macFUSE website (status {status})"
            ))
        }
    }

    pub fn run() {
        let result = tauri::Builder::default()
            .invoke_handler(tauri::generate_handler![
                frontend_bootstrap,
                open_macfuse_install_page
            ])
            .run(tauri::generate_context!());

        if let Err(error) = result {
            eprintln!("failed to run the quicKFS desktop client: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("quickfs-client-gui is currently available only on macOS");
}
