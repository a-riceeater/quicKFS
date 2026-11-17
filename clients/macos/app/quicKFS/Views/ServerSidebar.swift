// SPDX-License-Identifier: Apache-2.0
import SwiftUI

struct ServerSidebar: View {
    @Environment(AppModel.self) private var model
    @State private var removing: ServerProfile?

    var body: some View {
        @Bindable var model = model
        List(selection: $model.selection) {
            ForEach(model.profiles) { profile in
                ServerRow(profile: profile, state: model.state(profile.id))
                    .tag(profile.id)
                    .contextMenu { menu(for: profile) }
            }
        }
        .listStyle(.sidebar)
        .overlay {
            if model.profiles.isEmpty {
                Placard(symbol: "plus.circle.dashed", title: "Add a server")
                    .onTapGesture { model.isAddingServer = true }
            }
        }
        .toolbar {
            ToolbarItem {
                Button("Add Server", systemImage: "plus") { model.isAddingServer = true }
                    .help("Add Server")
            }
        }
        .confirmationDialog(
            "Remove \(removing?.name ?? "")?", isPresented: .init(
                get: { removing != nil }, set: { if !$0 { removing = nil } }),
            presenting: removing
        ) { profile in
            Button("Remove", role: .destructive) {
                Task { await model.remove(profile.id, forgetIdentity: false) }
            }
            Button("Remove and Forget Identity", role: .destructive) {
                Task { await model.remove(profile.id, forgetIdentity: true) }
            }
        } message: { _ in
            Text("Its saved password is deleted. Forgetting the identity means pairing again later.")
        }
    }

    @ViewBuilder
    private func menu(for profile: ServerProfile) -> some View {
        let state = model.state(profile.id)
        Button(state.mount.isActive ? "Unmount" : "Mount",
               systemImage: state.mount.isActive ? "eject" : "externaldrive.badge.plus") {
            model.toggleMount(profile.id)
        }
        if state.mount.isMounted {
            Button("Open in Finder", systemImage: "folder") { state.mount.revealInFinder() }
        }
        Button(state.connection.isOnline ? "Disconnect" : "Connect",
               systemImage: state.connection.isOnline ? "bolt.slash" : "bolt") {
            if state.connection.isOnline {
                state.connection.disconnect()
            } else {
                Task { try? await state.connection.connect() }
            }
        }
        Divider()
        Button("Remove…", systemImage: "trash", role: .destructive) { removing = profile }
    }
}

private struct ServerRow: View {
    let profile: ServerProfile
    let state: ServerState

    var body: some View {
        HStack(spacing: 10) {
            ServerGlyph(condition: state.condition, size: 30)
            VStack(alignment: .leading, spacing: 1) {
                Text(profile.name)
                    .font(.body.weight(.medium))
                    .lineLimit(1)
                Text(profile.host)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 4)
            if state.condition != .idle {
                StatusDot(condition: state.condition)
            }
        }
        .padding(.vertical, 3)
        .animation(.smooth, value: state.condition)
    }
}
