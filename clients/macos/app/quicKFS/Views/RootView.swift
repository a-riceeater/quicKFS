// SPDX-License-Identifier: Apache-2.0
import SwiftUI

struct RootView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        Group {
            if !model.macfuseInstalled {
                MacFUSEGate()
            } else {
                NavigationSplitView {
                    ServerSidebar()
                        .navigationSplitViewColumnWidth(min: 210, ideal: 240, max: 320)
                } detail: {
                    if let id = model.selection, let profile = model.profile(id) {
                        ServerDetail(profile: profile, state: model.state(id))
                            .id(id)
                    } else {
                        Placard(symbol: "externaldrive.badge.wifi", title: "No server selected") {
                            Button("Add Server", systemImage: "plus") { model.isAddingServer = true }
                                .buttonStyle(.glassProminent)
                                .controlSize(.large)
                        }
                    }
                }
            }
        }
        .frame(minWidth: 820, minHeight: 520)
        .sheet(isPresented: $model.isAddingServer) {
            AddServerSheet()
        }
        .sheet(item: $model.passwordRequest) { id in
            if let profile = model.profile(id) {
                PasswordSheet(profile: profile, mountAfter: true)
            }
        }
    }
}

/// Shown instead of everything else until macFUSE is installed.
struct MacFUSEGate: View {
    @Environment(AppModel.self) private var model
    @Environment(\.openURL) private var openURL
    @State private var checking = false

    var body: some View {
        Placard(
            symbol: "puzzlepiece.extension",
            title: "macFUSE is required",
            detail: "Install macFUSE, approve its system extension, then check again.",
            tint: .accentColor
        ) {
            Button("Install macFUSE", systemImage: "arrow.down.circle") {
                let link = model.environment?.macfuseInstallUrl ?? "https://macfuse.io/"
                if let url = URL(string: link) { openURL(url) }
            }
            .buttonStyle(.glassProminent)
            .controlSize(.large)

            Button {
                Task {
                    checking = true
                    await model.bootstrap()
                    checking = false
                }
            } label: {
                if checking { ProgressView().controlSize(.small) } else { Text("Check Again") }
            }
            .buttonStyle(.glass)
            .controlSize(.large)
        }
        .frame(minWidth: 520, minHeight: 380)
    }
}
