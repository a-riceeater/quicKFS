// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Servers on the side (or first, on a phone), the selected server's files
/// beside them. Collapses to one navigation stack on iPhone.
struct RootView: View {
    @Environment(AppModel.self) private var model
    @State private var visibility = NavigationSplitViewVisibility.automatic

    var body: some View {
        @Bindable var model = model
        NavigationSplitView(columnVisibility: $visibility) {
            ServerList()
        } detail: {
            if let id = model.selection, let profile = model.profile(id) {
                ServerBrowser(profile: profile, state: model.state(id))
                    .id(id)
            } else {
                Placard(symbol: "externaldrive.connected.to.line.below",
                        title: model.profiles.isEmpty ? "No servers" : "Choose a server") {
                    if model.profiles.isEmpty {
                        Button("Add Server", systemImage: "plus") { model.isAddingServer = true }
                            .glassButton(prominent: true)
                    }
                }
            }
        }
        .sheet(isPresented: $model.isAddingServer) { AddServerView() }
        .sheet(item: $model.passwordRequest) { id in
            if let profile = model.profile(id) { PasswordSheet(profile: profile) }
        }
    }
}
