// SPDX-License-Identifier: Apache-2.0
import ServiceManagement
import SwiftUI

/// Mount and eject from the menu bar without opening the window.
struct MenuBarView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(spacing: 10) {
            if model.profiles.isEmpty {
                Placard(symbol: "externaldrive.badge.plus", title: "No servers")
                    .frame(height: 140)
            } else {
                GlassEffectContainer(spacing: 6) {
                    VStack(spacing: 6) {
                        ForEach(model.profiles) { profile in
                            MenuServerRow(profile: profile, state: model.state(profile.id))
                        }
                    }
                }
            }
            Divider()
            HStack {
                Button("Open quicKFS", systemImage: "macwindow") {
                    openWindow(id: "main")
                    NSApp.activate()
                }
                Spacer()
                SettingsLink { Image(systemName: "gearshape") }
                    .help("Settings")
                Button("Quit", systemImage: "power") { NSApp.terminate(nil) }
                    .help("Quit quicKFS")
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.borderless)
            .font(.title3)
        }
        .padding(12)
        .frame(width: 300)
    }
}

private struct MenuServerRow: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model

    var body: some View {
        HStack(spacing: 10) {
            ServerGlyph(condition: state.condition, size: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text(profile.name).lineLimit(1)
                Text(profile.host).font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            Spacer()
            if state.mount.isMounted {
                Button("Open", systemImage: "folder") { state.mount.revealInFinder() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .help("Open in Finder")
            }
            Toggle("Mounted", isOn: Binding(
                get: { state.mount.isActive },
                set: { _ in model.toggleMount(profile.id) }))
                .toggleStyle(.switch)
                .controlSize(.small)
                .labelsHidden()
                .disabled(state.mount.phase == .stopping)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
        .glassEffect(.regular.interactive(), in: .rect(cornerRadius: 14))
        .help(state.failure ?? state.condition.help)
    }
}

struct SettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(Updater.self) private var updater
    @State private var launchAtLogin = SMAppService.mainApp.status == .enabled
    @State private var loginError: String?

    var body: some View {
        @Bindable var updater = updater
        Form {
            Section {
                Toggle("Open at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { setLaunchAtLogin(launchAtLogin) }
                if let loginError {
                    Text(loginError).font(.caption).foregroundStyle(.red)
                }
            }

            Section("Updates") {
                if updater.isConfigured {
                    Toggle("Check automatically", isOn: $updater.automaticallyChecks)
                    Toggle("Download automatically", isOn: $updater.automaticallyDownloads)
                        .disabled(!updater.automaticallyChecks)
                    LabeledContent {
                        Button("Check Now") { updater.checkForUpdates() }
                            .disabled(!updater.canCheck)
                    } label: {
                        if let last = updater.lastChecked {
                            Text("Checked \(last, style: .relative) ago")
                        } else {
                            Text("Never checked")
                        }
                    }
                } else {
                    Label("This build is not signed for updates", systemImage: "seal")
                        .foregroundStyle(.secondary)
                }
            }

            Section {
                LabeledContent("Version", value: Bundle.main.version)
                LabeledContent("Core", value: model.environment?.coreVersion ?? "")
                LabeledContent("macFUSE") {
                    Image(systemName: model.macfuseInstalled ? "checkmark.circle.fill" : "xmark.circle.fill")
                        .foregroundStyle(model.macfuseInstalled ? .green : .red)
                }
                LabeledContent("Data") {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([Paths.applicationSupport])
                    }
                }
            }
        }
        .formStyle(.grouped)
        .frame(width: 440)
        .fixedSize(horizontal: false, vertical: true)
    }

    private func setLaunchAtLogin(_ enabled: Bool) {
        do {
            if enabled { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            loginError = nil
        } catch {
            loginError = error.localizedDescription
            launchAtLogin = SMAppService.mainApp.status == .enabled
        }
    }
}

extension Bundle {
    var version: String {
        let short = infoDictionary?["CFBundleShortVersionString"] as? String ?? "0"
        let build = infoDictionary?["CFBundleVersion"] as? String ?? "0"
        return "\(short) (\(build))"
    }
}
