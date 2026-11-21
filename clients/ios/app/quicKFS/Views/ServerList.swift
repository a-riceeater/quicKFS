// SPDX-License-Identifier: Apache-2.0
import SwiftUI

struct ServerList: View {
    @Environment(AppModel.self) private var model
    @State private var removing: ServerProfile?
    @State private var showingSettings = false

    var body: some View {
        @Bindable var model = model
        List(selection: $model.selection) {
            ForEach(model.profiles) { profile in
                ServerRow(profile: profile, state: model.state(profile.id))
                    .tag(profile.id)
                    .swipeActions {
                        Button("Remove", systemImage: "trash", role: .destructive) { removing = profile }
                    }
                    .contextMenu {
                        Button("Connect", systemImage: "bolt.horizontal") {
                            Task { try? await model.state(profile.id).connection.connect() }
                        }
                        Button("Disconnect", systemImage: "bolt.horizontal.slash") {
                            model.state(profile.id).connection.disconnect()
                        }
                        Divider()
                        Button("Remove", systemImage: "trash", role: .destructive) { removing = profile }
                    }
            }
        }
        .navigationTitle("quicKFS")
        .overlay {
            if model.profiles.isEmpty {
                Placard(symbol: "server.rack", title: "No servers",
                        detail: "Add a quicKFS server to browse and stream its files.") {
                    Button("Add Server", systemImage: "plus") { model.isAddingServer = true }
                        .glassButton(prominent: true)
                }
            }
        }
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                Button("Settings", systemImage: "gearshape") { showingSettings = true }
            }
            ToolbarItem(placement: .topBarTrailing) {
                Button("Add Server", systemImage: "plus") { model.isAddingServer = true }
            }
        }
        .sheet(isPresented: $showingSettings) { AppSettingsView() }
        .confirmationDialog(
            "Remove \u{201C}\(removing?.name ?? "")\u{201D}?", isPresented: .init(
                get: { removing != nil }, set: { if !$0 { removing = nil } }),
            titleVisibility: .visible
        ) {
            Button("Remove", role: .destructive) {
                if let removing { Task { await model.remove(removing.id, forgetIdentity: false) } }
            }
            Button("Remove and Forget Identity", role: .destructive) {
                if let removing { Task { await model.remove(removing.id, forgetIdentity: true) } }
            }
        } message: {
            Text("The saved password is deleted. Forgetting the identity means pairing again to reconnect.")
        }
    }
}

private struct ServerRow: View {
    let profile: ServerProfile
    let state: ServerState

    var body: some View {
        HStack(spacing: 14) {
            ServerGlyph(condition: state.condition)
            VStack(alignment: .leading, spacing: 3) {
                Text(profile.name)
                    .font(.body.weight(.semibold))
                    .lineLimit(1)
                Text("\(profile.username)@\(profile.host)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if let latency = state.connection.latency.last, state.connection.isOnline {
                Text(Format.milliseconds(latency))
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.secondary)
            }
            StatusDot(condition: state.condition)
        }
        .padding(.vertical, 4)
        .animation(.smooth, value: state.condition)
    }
}
