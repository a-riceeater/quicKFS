// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Size of the offline read cache and a way to empty it.
///
/// The cache is shared by every server's mounts, so clearing it is only
/// possible while nothing is mounted; a live mount holds its cache files open.
struct CacheControl: View {
    @Environment(AppModel.self) private var model
    @State private var confirming = false
    @State private var working = false
    @State private var message: String?

    var body: some View {
        LabeledContent {
            HStack(spacing: 10) {
                if let message {
                    Text(message).font(.caption).foregroundStyle(.secondary)
                }
                Button("Clear…", role: .destructive) { confirming = true }
                    .disabled(working || model.activeMounts > 0 || (model.cacheUsage?.bytes ?? 0) == 0)
                    .help(model.activeMounts > 0 ? "Eject all servers to clear the cache" : "Delete cached data for every server")
            }
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text("Offline cache")
                if let usage = model.cacheUsage {
                    Text(usage.bytes == 0 ? "Empty" : "\(Format.bytes(usage.bytes)) · \(usage.files.formatted()) files · \(usage.namespaces) \(usage.namespaces == 1 ? "server" : "servers")")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
        }
        .task { await model.refreshCacheUsage() }
        .confirmationDialog("Clear the offline cache?", isPresented: $confirming) {
            Button("Clear Cache", role: .destructive) {
                working = true
                Task {
                    do {
                        let freed = try await model.clearCache()
                        message = "\(Format.bytes(freed)) freed"
                    } catch {
                        message = error.localizedDescription
                    }
                    working = false
                }
            }
        } message: {
            Text("Cached file data for every server is deleted. Files load from the server again the next time they are opened, and are unavailable offline until then.")
        }
    }
}
