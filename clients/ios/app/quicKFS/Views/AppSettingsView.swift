// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// App-wide settings: how much the streaming cache may keep, and what is in it.
struct AppSettingsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var confirming = false
    @State private var working = false
    @State private var message: String?

    var body: some View {
        @Bindable var model = model
        NavigationStack {
            Form {
                Section {
                    Picker("Keep up to", selection: $model.streamCacheGiB) {
                        ForEach([0.5, 1, 2, 4, 8, 16, 32], id: \.self) { size in
                            Text(Format.bytes(UInt64(size * 1_073_741_824))).tag(size)
                        }
                    }
                    LabeledContent("In use") {
                        Text(usage).monospacedDigit()
                    }
                    Button("Clear Cache", role: .destructive) { confirming = true }
                        .disabled(working || (model.cacheUsage?.bytes ?? 0) == 0)
                } header: {
                    Text("Streaming cache")
                } footer: {
                    Text(message ?? "Videos, music, and images keep what they have streamed, so seeking back or watching again is instant. Each server keeps up to this much; iOS may also clear it when storage runs low.")
                }
                Section {
                    LabeledContent("Version", value: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "")
                    if let core = model.environment?.coreVersion {
                        LabeledContent("Core", value: core)
                    }
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .task { await model.refreshCacheUsage() }
            .confirmationDialog("Clear the streaming cache?", isPresented: $confirming, titleVisibility: .visible) {
                Button("Clear Cache", role: .destructive) {
                    working = true
                    Task {
                        do {
                            let freed = try await model.clearCache()
                            message = "\(Format.bytes(freed)) freed."
                        } catch {
                            message = error.localizedDescription
                        }
                        working = false
                    }
                }
            } message: {
                Text("Streamed data for every server is deleted and loads from the server again when opened.")
            }
        }
    }

    private var usage: String {
        guard let usage = model.cacheUsage else { return "…" }
        if usage.bytes == 0 { return "Empty" }
        return "\(Format.bytes(usage.bytes)) · \(usage.namespaces) \(usage.namespaces == 1 ? "server" : "servers")"
    }
}
